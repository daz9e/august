# August

A minimal, modular core for building your own agent from the ground up.

August's core only provides mechanisms: it reaches people through messengers, calls models
through providers, runs turns, and fires events that can be hooked. 

Everything else lives in extensions: tools, commands, approvals, memory, sub-agents, and even Telegram and the
terminal. An extension is a separate process written in any language. It talks to the core
with one JSON object per line over stdin/stdout.

The agent can write extensions too, so it can grow behaviour nobody planned in advance.

> Looking for a ready-to-run agent? [august-agent](https://github.com/daz9e/august-agent)
> bundles the default extensions: Telegram and the terminal, Anthropic, OpenAI and other
> providers, approvals, memory, sub-agents, a scheduler and more.

## The idea

- **Messengers** are the drivers. Each one connects a piece of the world (Telegram, a
  terminal) and describes what it can do: Markdown, buttons, files, threads, edits.
- **Providers** are where models come from. Each one describes its models and streams
  their replies.
- **Extensions** are user space: tools, slash commands and hooks on events. An extension
  can offer a messenger or a provider.

The core knows no vendor and no feature. If a feature seems to need a special case in the
core, a general primitive is missing, and the core gets that primitive instead.

## Teach your agent something new

Your agent runs on a server, and you want to OK every shell command it runs, from your
phone, with a button. In August that's one extension.
**The core has no idea what an "approval" is: the extension hooks the tool call, sends you a question in Telegram (or any messenger), waits for your press, and lets the command through or blocks it.**

With the TypeScript SDK from august-agent, it's a dozen lines.
`~/.august/extensions/ask-me/index.ts`:

```ts
import type { August } from "august";

export default function (august: August) {
  august.describe("Asks before shell commands", "Every bash call waits for Run or Block in the home chat.");
  august.needs("messaging");
  august.on("tool_call", async ({ tool, input }) => {
    if (tool !== "bash") return;
    const answer = await august.ask("home", `Run \`${input.command}\`?`, ["Run", "Block"]);
    if (answer !== "Run") return { block: "the user said no" };
  }, { timeout: 310_000 });
}
```

`~/.august/extensions/ask-me/extension.json`:

```json
{"command": ["bun", "run", "../.runtime/ts/host.ts", "index.ts", "ask-me"]}
```

No SDK for your language? It's plain JSON lines over stdin/stdout; see the
[complete extension in Python](PROTOCOL.md#14-a-complete-extension).

Run `/reload`, and the next `rm -rf` waits for you on your phone.

> **Would you rather have new extensions picked up on their own? Write an extension for that!**

The same protocol covers the rest: tools, slash commands, hooks on every step of a turn
(rewrite the prompt, swap the model, trim the context), sub-agents, and whole new
messengers and model providers. See [PROTOCOL.md](PROTOCOL.md).

## Repository

| Path | What |
|---|---|
| `src/` | The core and the `august` binary |
| `sdk/` | `august-ext`: the Rust SDK for extensions, plus `FakeAugust` for testing them without the core |
| [PROTOCOL.md](PROTOCOL.md) | The extension protocol (version 2): the full contract between the core and extensions |

```sh
cargo build
cargo test
```

## Status

Early. The protocol is versioned; everything else may change.
