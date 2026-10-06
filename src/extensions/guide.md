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
    if (tool === "shell" && /git push --force/.test(input.command)) return { block: "force push is not allowed" };
  });
  august.on("turn_end", async ({ text, reply }, ctx) => { /* observe */ });
}
```

## Events

Handlers get `(data, ctx)`. Returned fields replace the event's data; return nothing to
leave it unchanged.

- `message_in` `{ text }`: a user message, before the agent sees it. Return `{ text }` to
  rewrite it, or `{ handled: true, reply? }` to swallow it.
- `before_turn` `{ text, system }`: once per turn. Return `{ system }` to change the base
  system prompt for this turn, `{ text }` to change the user message.
- `tool_call` `{ tool, input }`: before any tool runs (built-in or extension). Return
  `{ block: "reason" }` to stop it, `{ input }` to change its arguments.
- `tool_result` `{ tool, input, output, isError }`: return `{ output }` to change what the
  model sees.
- `turn_end` `{ text, reply }`: after a turn; runs in the background, the result is ignored.

`ctx.chat` is `{ channel, chat }` (`cli` in the terminal); `ctx.send(text)` messages that
chat, `ctx.prompt(text)` queues a new agent turn there. `august.send(channel, chat, text)`
and `august.prompt(...)` do the same for any chat.

## Rules

- Install or update an extension with `save_extension`; it loads it at once and reports
  errors. Fix and save again until it loads. The user can see all extensions with
  `/extensions` and reload them with `/reload`.
- Extensions run with full access to the machine, outside the workspace sandbox and the
  shell approvals. Write only what the user asked for.
- stdout is reserved for the protocol: log with `console.log`/`console.error` (goes to
  August's log).
- npm packages: just import them; bun installs them on first run. Keep state in files under
  `august.dir`.
- Timeouts: hooks 10 s, commands 60 s, tools 10 min. A failing or slow hook is skipped
  (August continues as if it returned nothing); a crashed extension is restarted.
- Built-in tool and command names can't be overridden.

## Full API types
