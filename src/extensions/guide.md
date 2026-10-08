Extensions add tools, slash commands and hooks to August itself. Use them when the user
asks you to change how you behave in a lasting way ("always...", "never...", "add a
/command that...", "when X happens, do Y"), or to add a capability that needs code.

## Shape

An extension is one TypeScript file, `<extensions dir>/<name>/index.ts`, run by bun in its
own process. It default-exports a setup function:

```ts
import type { August } from "august";

export default function (august: August) {
  // A tool the model can call.
  august.registerTool({
    name: "weather",
    description: "Current weather for a city",
    parameters: { type: "object", properties: { city: { type: "string" } }, required: ["city"] },
    async execute({ city }) {
      return await (await fetch(`https://wttr.in/${encodeURIComponent(city)}?format=3`)).text();
    },
  });

  // A slash command: /standup
  august.registerCommand("standup", {
    description: "Post the standup template",
    handler: async (args, ctx) => "Yesterday:\nToday:\nBlockers:",
  });

  // Hooks.
  august.on("tool_call", ({ tool, input }) => {
    if (tool === "bash" && /git push --force/.test(input.command)) return { block: "force push is not allowed" };
  });
  august.on("turn_end", async ({ text, reply }, ctx) => { /* observe */ });
}
```

## Events

Handlers get `(data, ctx)`. Returned fields replace the event's data; return nothing to
leave it unchanged.

- `message_in` `{ id, text, files, source }`: a message for the thread, before the agent
  sees it; `source` is `user` for what came from a messenger, else a `prompt`'s source. `files` are its
  attachments, already saved (`{ path, mime, voice }`). Return `{ text }` to rewrite it, or
  `{ handled: true, reply? }` to swallow it.
- `message_out` `{ kind: send|edit, id, text, buttons, files, reply_to }`: before August
  sends or edits any message (replies, command answers, questions, extensions' `send`);
  a streamed reply passes once per edit. Return changed fields, or `{ block: true }` to drop
  the message.
- `before_turn` `{ text, system }`: once per turn. Return `{ system }` to change the base
  system prompt for this turn, `{ text }` to change the user message.
- `tool_call` `{ tool, input, id, caller }`: before any tool runs (built-in or extension);
  `id` is the model's call id (null outside a model call), `caller` is `model`, or
  `ext:<name>` for an extension's `callTool`. Return
  `{ block: "reason" }` to stop it, `{ input }` to change its arguments. The handler may
  take its time (raise its timeout with `hook_timeout`): ask the user, run a check with
  `ctx.llm`, then let the call through or block it. That is all approvals are: the default
  `approvals` extension does exactly this.
- `tool_result` `{ tool, input, id, caller, output, isError }`: return `{ output }` to change what the
  model sees.
- `turn_end` `{ text, reply, status, toolCalls, unattended }`: after any turn; `status` is
  `ok`, `error` or `cancelled`, `toolCalls` how many tools it called, and `ctx.turn` its
  `{ id, mode, source, parent }` (`unattended`: not the user's visible conversation). Runs
  in the background; the result is ignored.
- `llm_call` `{ step, system, model, tools }`: before every model call of a turn (`step`
  from 0; `tools` are names). For that call only, return `{ system }` to use another system
  prompt, `{ tools }` to offer only some of the tools, `{ model }` to use another model of
  the active provider or `provider:model`. The prompt is otherwise
  byte-stable so the provider can cache it; changing it costs that cache, so prefer
  `before_turn`.
- `model_select` `{ model, previous }`: the model is being switched (`/model`,
  `model_set`). Return `{ model }` to switch to another one, `{ block: "reason" }` to refuse.
- `context` `{ step, messages }`: before every model call, the conversation the model is
  about to see. Return `{ messages }` to change it for that call only (inject recalled notes,
  drop noise); the stored history stays as is. Keep tool_use/tool_result pairs intact.
- `llm_result` `{ step, text, toolCalls: [{ name, input }], usage }`: after every model call.
- `turn_event` `{ kind: text|step|tool|compacted, text?, tool?, input? }`: what a visible
  turn does, in order, as it happens (reply fragments, a new model call, a tool call, a
  compaction): the stream a renderer draws from.
- `turn_settled` `{}`: nothing runs in the thread any more and nothing is about to (every
  turn ended, no message waits, `turn_end` handlers are done); once per quiet period. Use it
  for "August is free" rather than `turn_end`, after which a queued message or an extension
  may continue.
- `stop` `{ turns }`: the user sent /stop; `turns` are the ids of the turns it cancels. Stop
  any loop of yours in that thread.
- `session_start` `{ session, previous, reason: start|new, chat }`: before a
  conversation's first turn (`start`: the thread's first, or after a restart with none;
  `new`: after `/new`). Return `{ model, system, tools }` to set the conversation's own
  settings, e.g. another model for one messenger's threads (`ctx.thread.messenger`).
- `session_changed` `{ reason: new|switch, previous, session }`: the thread's conversation
  was replaced, right away (drop per-conversation state here).
- `session_before_new` / `session_before_switch` `{ session, to, by }`: `/new` or a switch
  is about to happen (`by`: `user` or `ext:<name>`). Return `{ block: "reason" }` to refuse.
- `session_before_compact` `{ reason: threshold|manual|overflow, tokens, messages,
  previousSummary, kept }`: older history (`messages`; the last `kept` stay as they are) is
  about to be summarised (`overflow`: the model refused a too-long context). Return
  `{ cancel: true }` to skip the summary, or `{ summary }` to write it yourself (another
  template, a cheaper model, saving facts on the way); a previous summary should be folded
  in.
- `compaction` `{ before, after, reason, fromExtension }`: older history was summarised
  (estimated tokens).

Hooks that may change data form a chain: extensions in the user's order (`hooks.order` in
`august.json`, then the rest by name), each returning only the fields it changes; the next
one sees the merged result, and `block` / `handled` ends the chain. `turn_end`,
`turn_event`, `turn_settled`, `llm_result`, `session_changed`, `compaction`, `reaction`, `extension_state`,
`config_changed` and `stop` only observe: every handler gets them at once, the results are
ignored, and all but `stop` run in the background.

Your own events: extensions hook each other the same way. Declare one with
`august.defineEvent("done", { description, schema, observe })` and run it with
`await ctx.emit("done", data)` (or `august.emit` outside a handler); others hook it as
`august.on("<your name>:done", ...)`. A chain event resolves to the data its handlers leave
(read `block` and the fields you allow them to change); an `observe` one returns at once.
The data must be an object matching `schema` (top-level `required` and `type`s are checked).
Keep dependencies soft: emit whether anyone listens or not, and hook another extension's
event only for extra behaviour — nothing arrives while it isn't running.
`august.extensions.list()` shows every extension's `events` with their descriptions and
schemas. An extension that stands in for another (your own `compaction`) calls
`august.replaces("compaction")` and emits `compaction:<event>`, so existing hooks keep
working.

`ctx.thread` is the thread the call belongs to: `{ messenger, id }`, e.g.
`{ messenger: "telegram", id: "123" }` or a terminal window `{ messenger: "cli", id: "1" }`.

Messengers and messages — August's primitives, usable for any thread:
- `await august.messengers()` lists every messenger with what it can do (`capabilities`:
  Markdown, buttons, edits, files, images, audio, threads, ...; plus free-form `extra`) and
  its threads, the one the user wrote in last marked `active`.
- `await august.send(thread, { text, buttons: [{ id, label }] })` sends a message (a plain
  string works too) and returns its id; `august.edit(thread, id, message)` replaces it.
  `ctx.send(message)` sends to `ctx.thread`. A message can also carry button rows
  (`buttons: [[...], [...]]`), local `files` (the text is their caption) and `reply_to` (an
  id). Where the messenger can: `august.delete(thread, id)`, and
  `august.react(thread, id, "👍")` on any message, the user's too (`message_in` has its id).
  The user's reactions arrive as the `reaction` event `{ message, emoji }`.
- To wait for the user: `const l = await august.listen(thread, { buttons: [...ids], text: true })`
  *before* sending the question, then `await august.next(l, { timeout: 60_000 })` gives
  `{ press: id }`, `{ text }`, `{ timeout: true }` or `{ cancelled: "stop" | "new" }`. What a
  listener takes doesn't reach the agent; one nobody collects ends after its `ttl`
  (default 10 minutes). You decide how long to wait and what to do
  without an answer (ask elsewhere, remind, give up).
- `await ctx.ask(question, ["Yes", "Later"], { timeout })` (or `august.ask(thread, ...)`)
  does all that: buttons, and the answer as the option pressed, numbered or named, the
  user's own words, or null.
- `ctx.prompt(text, opts)` / `august.prompt(thread, text, opts)` hand a thread a message.
  `source` says who it is from (default `ext:<your name>`; the model sees `[from <source>]`,
  `user` passes it as the user's); `deliver` is `steer` (default: joins the running turn
  before its next model call, or starts one), `followUp` (its own turn after the current
  one) or `nextTurn` (waits for the next turn without starting one). It passes `message_in`
  like a user's message, with its `source`.

Calling into August:
- `await ctx.callTool("read", { path: "notes.md" })` runs any agent tool (built-in,
  MCP or another extension's) for that thread, through the `tool_call`/`tool_result` hooks
  returns `{ output, isError }`.
- `await ctx.llm(prompt, { system })` is one completion on the current model, without tools;
  returns the text.
- `await ctx.agent(task, { system, tools, exclude })` runs a sub-agent with a fresh
  conversation and returns its final reply (`tools` limits it, `exclude` hides some).
- Turns: `const id = await august.turns.start(thread, { text, mode })` starts a `quiet` turn
  (in the thread's conversation, nothing shown, e.g. a scheduled check), a `fork` (on a copy
  of the conversation, nothing kept; `tools` limits what it may call; good for looking back
  at a conversation) or a `fresh` one (a sub-agent); `await august.turns.wait(id)` gives
  `{ status, reply, error, toolCalls }`. `august.turns.cancel(id)`, `august.turns.list()`.
  /stop cancels every turn of its thread. `ctx.turn` tells which turn a call runs in
  (`{ id, mode, source, parent }`).
- `august.workspace` is the agent's workspace folder.

State: `august.store` keeps JSON values by key in August's database, across restarts
(`await august.store.get(key)`, `set(key, value)`, `delete(key)`, `list(prefix)`); only your
extension sees them. Files under `august.dir` work too.

`august.registerPromptSection(name, text)` adds a section to the system prompt (e.g.
"## Reminders" with when to use your tools). The prompt is fixed for a conversation so the
provider can cache it: a new or changed section shows up from the next conversation.

Tools can be registered (and removed with `august.unregisterTool(name)`) at any time, not
only during setup, e.g. once a remote service answers; the model sees them from its next
call.

A tool with the name of a built-in one (`bash`, `read`, `write`, `edit`) replaces it, e.g. to run
bash commands in a container.

## Operations

Everything August can be asked to do is one table of operations; slash commands like
`/model` or `/new` are thin wrappers around the same ones. `await august.ops()` lists them
with the permission each needs; `await august.call(name, params)` calls any of them. Typed
helpers for the common ones:
- `august.tools()`, `august.commands()`: every agent tool and slash command, with its
  `owner` (`august` or the extension).
- `august.status(thread?)`: `{ provider, model, workspace, busy }`.
- `august.model.set(id)`: switch the model, like `/model id`.
- `august.sessions.new(thread, { name, settings })`, `.compact(thread)`, `.usage(thread)`:
  like `/new`, `/compact`, `/usage`.
- Conversations are stored and addressable: `august.sessions.list(thread?)`,
  `.switch(thread, id)` continues one in a thread, `.update(id, { name, settings })`. A
  conversation's journal — turns, messages, model replies, every tool call (also the ones
  extensions make with `callTool`, with their `caller`), compactions, session and model
  changes — is `august.sessions.history({ thread } | { session }, { kinds, since, limit })`;
  `august.journal.append({ thread }, type, data)` adds an entry of your own (kind
  `custom`). A conversation's `settings` — `model` (`provider:model` or a model of the active provider),
  `system` (added to the system prompt), `tools` (only these) — apply from its next turn.
- `august.stop(thread)`: like `/stop`.
- `august.search(query, limit?)`: full-text search over everything said in any conversation.
- `august.extensions.list()`, `.enable(name)`, `.disable(name)`, `.reload()`: like
  `/extensions` and `/reload`.

## Settings

An extension's settings live in `config/extensions/<name>.json` (with `enabled` and
`origin`, which only the user and August write). Declare them with a JSON Schema in setup,
and read them when you need them:

```ts
august.settings.schema({
  type: "object",
  properties: {
    city: { type: "string", default: "Berlin" },
    api_key: { type: "string", secret: true },
  },
});
const { city, api_key } = await august.settings.get();
```

The user sets them with `/config extensions.<name>.settings.city Paris` (or
`august config ...` in a terminal); secrets show as `••••`. Hook `config_changed { path }`
to react at once. `august.settings.set(path, value)` changes your own. Don't keep keys in
code or in the store: ask the user to put them into your settings.

Where a call takes a thread, `"home"` is the user's home thread (set with `/home`, else the
thread they wrote in last), e.g. `august.send("home", "...")` for reports nobody asked for.
`extension_state { name, state, error }` tells when an extension started, failed, crashed or
was turned off.

## Permissions

An extension declares what it uses beyond the thread of the call in progress, and August
refuses the rest: `august.needs("messaging", "turns")` in its setup.
- `messaging`: messengers and any thread (`august.send`/`listen`/`prompt` elsewhere, or later);
- `turns`: `august.turns`, `ctx.agent`, `august.stop`;
- `tools`: `ctx.callTool`;
- `llm`: `ctx.llm`;
- `models`: `august.model.set`;
- `sessions`: `august.sessions.*`, `august.search`;
- `config`: `august.config.get/set` (any unit's settings);
- `admin`: `august.extensions.*` (other extensions: list, enable, disable, reload; a reload restarts every extension but yours).
- `user`: act for the user, as a slash command does: `august.call(op, { ...params, as_user: true })`
  counts as the user's (hooks see `by: "user"`, and it may turn extensions on and off).

Answering in the call's own thread while it runs (`ctx.send`, `ctx.ask`), the store,
your own settings, and reading `ops`, `tools`, `commands`, `status` need
nothing. `/extensions`
shows what each one needs. Ask for no more than the extension uses.

## Rules

- Install or update an extension with `save_extension`; it loads it at once and reports
  errors. Fix and save again until it loads. The user can see all extensions with
  `/extensions`, reload them with `/reload`, and pause one with
  `/extensions disable <name>` (`enable` starts it again; saving it also re-enables it).
- Extensions run with full access to the machine, outside the workspace sandbox and the
  approvals of the agent's tool calls. Write only what the user asked for.
- stdout is reserved for the protocol: log with `console.log`/`console.error` (goes to
  August's log).
- npm packages: just import them; bun installs them on first run. Keep state in files under
  `august.dir`.
- Timeouts: hooks 10 s (`message_in` 2 min; `august.on(event, handler, { timeout })` sets
  your own), commands 60 s, tools 10 min, setup 30 s. When August stops waiting for a call
  (its turn was cancelled with /stop, or it timed out), `ctx.signal` is aborted: pass it to
  `fetch` and long work so it stops too.
- Before a reload or `/extensions disable`, the `shutdown` event gives you 2 s to clean up. A failing or slow hook is skipped
  (August continues as if it returned nothing); a crashed extension is restarted.
- Built-in command names can't be taken; a tool with a built-in tool's name replaces it.

## Full API types
