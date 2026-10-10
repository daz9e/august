Extensions add tools, slash commands and hooks to August itself: write one when a task
needs a capability you don't have (a tool, a hook, a /command, a background job), or when the
user asks you to behave differently in a lasting way that takes code ("when X happens, do Y").
A fact or a preference to remember is not an extension; a procedure is a skill.

Build the capability, not the one task. Before writing, check `august.tools()` and
`august.extensions.list()`: maybe it exists. Then write the general piece the task is one use
of — a tool with parameters (the thread, the text, the schedule), not one with this task's
values baked in — and use it for the task right after saving it. No one-off code that runs once
in setup, no "done" flags in the store: an extension stays installed, so it must stay useful.
For example, "make the agent in my terminal create a file and tell me" is a `hand_off` tool
that runs a task in any thread and reports back (see Turns below), called once.

To change what a default extension does (`render`, `approvals`, `memory`, ...), save one of
your own under its name: it replaces the default.

This guide and the types at the end are the whole API; you don't need August's source code.

## Shape

An extension is one TypeScript file, `<extensions dir>/<name>/index.ts`, run by bun in its
own process. It default-exports a setup function:

```ts
import type { August } from "august";

export default function (august: August) {
  // What it does: one line, then the details (required).
  august.describe(
    "Weather tool and a /standup template",
    "`weather` fetches the current weather for a city from wttr.in. /standup posts an empty \
standup template. Blocks `git push --force` in bash.",
  );

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

- `message_in` `{ id, text, files, source, deliver, steer }`: a message for the thread, before
  the agent sees it; `source` is `user` for what came from a messenger, else a `prompt`'s
  source (the default `conversation` extension marks it `[from <source>]`). `files` are its
  attachments as the messenger has them, for `august.download(thread, file, path)`; the
  default `attachments` extension saves them into `workspace/inbox` and leaves
  `{ path, mime, kind, voice }` for later hooks. `steer`: it would join the turn running now
  (`conversation` acknowledges it and marks it for the model). Return `{ text }` to rewrite
  it, `{ images: [{ path, mime }] }` to show the model images (such a message starts its own
  turn), `{ deliver }` to change how it arrives, or `{ handled: true, reply? }` to swallow it.
- `message_out` `{ kind: send|edit, id, text, buttons, files, reply_to }`: before August
  sends or edits any message (replies, command answers, questions, extensions' `send`);
  a streamed reply passes once per edit. Return changed fields, or `{ block: true }` to drop
  the message.
- `before_turn` `{ text, system }`: once per turn. Return `{ system }` to change the base
  system prompt for this turn, `{ text }` to change the user message. The core's base prompt
  is empty: the default `conversation` extension writes who August is and timestamps the text.
- `tool_call` `{ tool, input, id, caller }`: before any tool runs;
  `id` is the model's call id (null outside a model call), `caller` is `model`, or
  `ext:<name>` for an extension's `callTool`. Return
  `{ block: "reason" }` to stop it, `{ input }` to change its arguments. The handler may
  take its time (raise its timeout with `hook_timeout`): ask the user, run a check with
  `ctx.llm`, then let the call through or block it. That is all approvals are: the default
  `approvals` extension does exactly this.
- `tool_result` `{ tool, input, id, caller, output, isError }`: return `{ output }` to change what the
  model sees.
- `turn_end` `{ text, reply, status, error, toolCalls, unattended }`: after any turn; `status` is
  `ok`, `error` (why: `error`) or `cancelled`, `toolCalls` how many tools it called, and `ctx.turn` its
  `{ id, mode, source, parent }` (`unattended`: not the user's visible conversation). Runs
  in the background; the result is ignored.
- `llm_call` `{ step, system, model, tools }`: before every model call of a turn (`step`
  from 0; `tools` are names). For that call only, return `{ system }` to use another system
  prompt, `{ tools }` to offer only some of the tools, `{ model }` to use another model of
  the active provider or `provider:model`, `{ effort }` (`low` … `high`, as the provider
  takes it) to think more or less, `{ options }` to set fields of the provider's request
  body (`{ temperature: 0 }`; the provider's own API names them). The prompt is otherwise
  byte-stable so the provider can cache it; changing it costs that cache, so prefer
  `before_turn`.
- `model_select` `{ model, previous }`: the model is being switched (`/model`,
  `model_set`). Return `{ model }` to switch to another one, `{ block: "reason" }` to refuse.
- `context` `{ step, messages, system, tokens, window, error }`: before every model call,
  the conversation the model is about to see, the input tokens the provider reported last
  (`tokens`, 0 before the first call) and the model's context `window` (null if unknown).
  Return `{ messages }` to change it for that call only (inject recalled notes, drop
  noise), or `{ history, note }` to replace the conversation itself from then on (stored;
  older messages stay searchable) and show `note` in the turn — the default `compaction`
  extension summarises older messages this way. Keep tool_use/tool_result pairs intact.
  `error` `{ kind, message }`: the previous try of this call failed and an `llm_error`
  handler asked for another.
- `llm_error` `{ step, attempt, model, error: { kind, message }, streamed }`: a model call
  failed (`kind`: `rate_limit`, `overloaded`, `network`, `auth`, `quota`, `context_too_long`,
  `refused`, `bad_request`, `other`; `streamed`: part of its reply was already shown, so
  another try would show it again). Return `{ retry: true }` to try again, with
  `delayMs` to wait first and `model` to use another one for the rest of the turn; at most
  8 tries.
- `llm_result` `{ step, session, text, toolCalls: [{ name, input }], usage }`: after every
  model call August makes — a turn's, a fork's, or an `llm` call's (`step` null); the
  default `usage` extension counts them for `/usage`.
- `turn_event` `{ kind: text|step|tool|note, text?, tool?, input? }`: what a visible
  turn does, in order, as it happens (reply fragments, a new model call, a tool call, a
  `note` a `context` handler asked to show): the stream a renderer draws from.
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
- The default `compaction` extension emits `compaction:before` `{ reason:
  threshold|manual|overflow, tokens, messages, previousSummary, kept }` before it summarises
  older history (`messages`; the last `kept` stay as they are). Return `{ cancel: true }` to
  skip the summary, or `{ summary }` to write it yourself (another template, a cheaper model,
  saving facts on the way); fold a previous summary in. Then `compaction:after` `{ before,
  after, reason, fromExtension }` (estimated tokens; observe only).

Hooks that may change data form a chain: extensions in the user's order (`hooks.order` in
`august.json`, then the rest by name), each returning only the fields it changes; the next
one sees the merged result, and `block` / `handled` ends the chain. `turn_end`,
`turn_event`, `turn_settled`, `llm_result`, `session_changed`, `reaction`, `extension_state`,
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

Jobs of the core: some things the core does only while no extension does them in its place.
`august.takes("render")` makes this extension draw the user's visible turns: it gets `render`
events one at a time, in order (the next waits for the handler; text arriving meanwhile
comes merged) and sends or edits messages in `ctx.thread` itself. `start` carries the
messenger's `capabilities` (`edit`, `edit_interval_ms`, `max_len`, ...); `text`, `step`,
`tool`, `note` are as in `turn_event`; `break` means a message (a file, a question) was
sent in the reply's place, so finish what is shown and continue in a new message below;
`end` `{ status, reply, error }` is the outcome. The default `render` extension streams the
reply by editing messages and shows a line per tool call; take `render` yourself to draw
differently. With nobody taking it, only each turn's outcome is sent. Of several, the first
in `hooks.order` gets it.

`ctx.thread` is the thread the call belongs to: `{ messenger, id }`, e.g.
`{ messenger: "telegram", id: "123" }` or a terminal window `{ messenger: "cli", id: "1" }`.

Messengers and messages — August's primitives, usable for any thread:
- `await august.messengers()` lists every messenger with what it can do (`capabilities`:
  Markdown, buttons, edits, files, images, audio, threads, ...), `notes` in prose, its own
  `actions` (described like tools) and its threads with their `place` (`dm`, `group`,
  `channel`, or a `thread` inside a `parent`), the one the user wrote in last marked `active`.
- What only one messenger can do (pin a message, ...) is one of its `actions`:
  `await august.action(thread, "pin", { message: id })`, the arguments checked against its
  schema. `await august.openThread(thread, "Title")` opens a thread inside one (a forum
  topic) where `capabilities.open_thread`.
- A message in a group may not be meant for August: `message_in` has `addressed: false`
  then, and nothing runs unless a hook sets it to true. `reply_to` `{ id, text, mine }` is the
  message it answers (the default `conversation` extension quotes it for the agent). An edit of the user's message arrives as the
  `message_edited` event `{ id, text }`.
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
  `source` says who it is from (default `ext:<your name>`; `user` passes it as the user's); `deliver` is `steer` (default: joins the running turn
  before its next model call, or starts one), `followUp` (its own turn after the current
  one) or `nextTurn` (waits for the next turn without starting one). It passes `message_in`
  like a user's message, with its `source`.

Calling into August:
- `await ctx.callTool("read", { path: "notes.md" })` runs any agent tool (
  MCP or another extension's) for that thread, through the `tool_call`/`tool_result` hooks
  returns `{ output, isError }`.
- `await ctx.llm(prompt, { system, effort, options })` is one completion without tools on
  the thread's conversation's model (`llm_result` counts it there); `effort` and `options`
  as `llm_call` takes them; `prompt` may be a list of messages as `context` has them.
  Returns the text.
- `await ctx.agent(task, { system, tools, exclude })` runs a sub-agent with a fresh
  conversation and returns its final reply (`tools` limits it, `exclude` hides some).
- Turns: `const id = await august.turns.start(thread, { text, mode })` starts a `visible`
  turn (in the thread's conversation, shown there like one of the user's, after whatever runs
  there now; `conversation` marks it `[from <source>]`; unlike `prompt`, it skips `message_in`), a `quiet` one (in the thread's conversation,
  nothing shown, e.g. a scheduled check), a `fork` (on a copy of the conversation, nothing
  kept; `tools` limits what it may call; good for looking back at a conversation) or a
  `fresh` one (a sub-agent); `await august.turns.wait(id)` gives
  `{ status, reply, error, toolCalls }`. `source` defaults to `ext:<your name>`, `parent`
  links it to the turn that asked (`ctx.turn.id`). `august.turns.cancel(id)`,
  `august.turns.list()`. /stop cancels every turn of its thread, queued ones too. `ctx.turn`
  tells which turn a call runs in (`{ id, mode, source, parent }`). Work in another thread
  that reports back:
  ```ts
  const id = await august.turns.start(target, { text: task, mode: "visible", parent: ctx.turn?.id });
  august.turns.wait(id).then((out) => august.prompt(ctx.thread!, `#${id} ${out.status}: ${out.reply}`, { deliver: "followUp" }));
  return `started #${id}`; // don't hold the tool call open while it runs
  ```
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

Every tool comes from an extension; `bash`, `read`, `write` and `edit` from the default `tools`
one. A tool of yours with one of those names replaces it (of two extensions offering a tool, a
user's own wins over a default), e.g. to run bash commands in a container.

## Operations

Everything August can be asked to do is one table of operations; slash commands like
`/model` or `/new` are thin wrappers around the same ones. `await august.ops()` lists them
with the permission each needs; `await august.call(name, params)` calls any of them. Typed
helpers for the common ones:
- `august.tools()`, `august.commands()`: every agent tool and slash command, with its
  `owner` (`august` or the extension).
- `august.status(thread?)`: `{ provider, model, workspace, busy }`.
- `august.model.set(id)`: switch the model, like `/model id`.
- `august.sessions.new(thread, { name, settings })`: like `/new`.
  `.messages(thread)` is the live conversation (what the model sees next, with `tokens` and
  `window`); `.setMessages(thread, messages)` replaces it (outside the thread's running turn;
  inside one, return `history` from `context`).
- A conversation needs no chat: `august.sessions.new(null, { name, settings })` starts one
  of its own and returns its id; the thread `{ messenger: "session", id }` addresses it, so
  `august.turns.start` runs quiet, fork and fresh turns in it (a background agent that keeps
  its context, a conversation between agents) and `august.sessions.messages` reads it.
  Nothing can be sent there; report back to a real thread.
- Conversations are stored and addressable: `august.sessions.list(thread?)`,
  `.switch(thread, id)` continues one in a thread, `.update(id, { name, settings })`. A
  conversation's journal — turns, messages, model replies, every tool call (also the ones
  extensions make with `callTool`, with their `caller`), session and model
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
code, settings or the store: declare an account (below).

## Accounts and sign-in

What the user signs in to (a model provider, a service with an API key or OAuth) is an
account, and August runs the sign-in: `/login` lists every account, asks where the user
is and keeps the secrets. An API key needs no code:

```ts
august.registerAccount({
  id: "weather", label: "Weather API", key: { label: "API key", env: "WEATHER_KEY" },
  check: async (key) => { if (!(await fetch(`https://api.example.com/ping?key=${key}`)).ok) throw new Error("rejected"); },
});
const key = process.env.WEATHER_KEY ?? (await august.secrets.get("weather"));
```

Anything else is a script of steps August shows to the user (`ask`, `choose`, `open`,
`progress`) and a redirect August receives on localhost (`callback`, `waitCallback`):

```ts
august.registerAccount({
  id: "github", label: "GitHub",
  async login(steps) {
    const redirect = await steps.callback({ path: "/github" });
    await steps.open(`https://github.com/login/oauth/authorize?client_id=…&redirect_uri=${encodeURIComponent(redirect)}&state=s1`);
    const q = await steps.waitCallback();
    if (q.state !== "s1") throw new Error("state mismatch");
    await august.secrets.set("github", await exchange(q.code));
    return { who: "…" };
  },
  async logout() { await august.secrets.set("github", null); },
});
```

When a token can't be refreshed any more, `august.accountUpdate("github", "expired")`: the
user is told to sign in again.

Where a call takes a thread, `"home"` is the user's home thread (set with `/home`, else the
thread they wrote in last), e.g. `august.send("home", "...")` for reports nobody asked for.
`extension_state { name, state, reason, error, detail, restarts, final }` tells when an
extension starts, runs, is degraded, fails, crashes or is turned off (`state`:
`starting|running|degraded|failed|disabled`; `reason`: `install|update|start|reload|enable|
restart|exit|hang|unhealthy|recovered|disable`; `final`: it won't be restarted).

## Lifecycle

August watches every extension. It asks `health` every 30 s: no answer twice → it hangs and
is restarted; register `august.health(() => ({ status, detail }))` to say more — `degraded`
when something outside is wrong (no key, no network; shown, not restarted), `failed` when
something inside broke (a loop that stopped; restarted). A crash is restarted after 1, 2,
4, … s, at most 5 times in a row. Each extension's stderr is logged by August
(`logs/extensions/<name>.log`); `august.call("extension_logs", { name, lines })` reads it.

Hooks on other extensions' lives need `admin`:
- `extension_launch` `{ name, dir, command, env, reason, changed }`: before its process
  starts (`reason` `install`/`update` when it is new or its files changed, with the
  `changed` files). Return `{ command, env }` to start it otherwise (a sandbox, secrets in
  the environment), `{ block: "why" }` to keep it from starting. May take long (a build).
- `extension_ready` `{ name, reason, changed, manifest }`: it started and registered
  (`manifest`: its tools, hooks, needs, ...) but gets nothing yet. `{ block }` stops it,
  `{ needs: [...] }` limits its permissions.
- `extension_exit` `{ name, reason: exit|hang|unhealthy, code, error, restarts, uptime_ms }`:
  its process ended. `{ restart: false }` keeps it down, `{ delay_ms }` sets the pause.
- `extension_output` `{ name, line }` (observe): every line it writes to stderr.
Mark a watcher `early` (`/config extensions.<name>.early true`) so it starts before the
extensions it watches.

## Any language

A folder with `extension.json` instead of `index.ts` is an extension in any language: August
runs its `command` in the folder and speaks the same protocol over stdin/stdout
(newline-delimited JSON; see the types at the end for the methods):

```json
{ "command": ["python3", "main.py"], "env": { "MODE": "fast" },
  "setup": [["pip", "install", "-r", "requirements.txt"]] }
```

`setup` (the default `setup` extension): commands run in the folder before it starts, when
it is new, when its files changed, or on `/setup <name>` — build it, download it, install
its dependencies. A failed step keeps it from starting with the step's output; `setup:step`
hooks may change or block each step.

Save it with `save_extension` and `files` (every file by path, `extension.json` included),
not by writing files yourself: August then runs its setup, starts it and tells you what it
registered, or the error with its log. Don't build or install by hand; put it in `setup`.
Run scripts through their interpreter (`["python3", "main.py"]`), and keep generated files
out of the folder's sources (`target/`, `node_modules/` are ignored when August checks for
changes). It's done when the result says it started, not before.

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
- `admin`: `august.extensions.*` (other extensions: list, enable, disable, reload; a reload restarts every extension but yours), their logs and health (`extension_logs`, `extension_health`), and hooks on their lives (`extension_launch`, `extension_ready`, `extension_exit`, `extension_output`).
- `user`: act for the user, as a slash command does: `august.call(op, { ...params, as_user: true })`
  counts as the user's (hooks see `by: "user"`, and it may turn extensions on and off).

Answering in the call's own thread while it runs (`ctx.send`, `ctx.ask`), the store,
your own settings, and reading `ops`, `tools`, `commands`, `status` need
nothing. `/extensions`
shows what each one needs. Ask for no more than the extension uses.

## Rules

- Every extension calls `august.describe(summary, details)`. The summary is one line the
  model sees each turn; the details say how it works and what it changes on its own
  (messages it sends, sessions it starts, what it blocks, timers), so whoever meets its
  effects later can tell where they came from. Keep them true when you change the code.
- Install or update an extension with `save_extension`; it loads it at once and reports
  errors. Fix and save again until it loads. The user can see all extensions with
  `/extensions`, reload them with `/reload`, and pause one with
  `/extensions disable <name>` (`enable` starts it again; saving it also re-enables it).
- Extensions run with full access to the machine, outside the workspace sandbox and the
  approvals of the agent's tool calls. Write only what the user asked for.
- stdout is reserved for the protocol: log with `console.log`/`console.error` (goes to
  August's log).
- npm packages: just import them; bun installs them on first run.
- Timeouts: hooks that may change data 10 s (`message_in` 2 min; `august.on(event, handler,
  { timeout })` sets your own), observe-only hooks 1 h, commands and tools 10 min, setup 30 s. When August stops waiting for a call
  (its turn was cancelled with /stop, or it timed out), `ctx.signal` is aborted: pass it to
  `fetch` and long work so it stops too.
- Before a reload or `/extensions disable`, the `shutdown` event gives you 2 s to clean up. A failing or slow hook is skipped
  (August continues as if it returned nothing); a crashed extension is restarted.
- Built-in command names can't be taken; a tool of yours replaces a default extension's tool of the same name.

## Full API types
