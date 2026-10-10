# August

A minimal, modular core for building your own agent from the ground up.

August's core only provides mechanisms: it reaches people through messengers, calls models
through providers, runs turns, and fires events that can be hooked. Everything else lives in
extensions: tools, commands, approvals, memory, sub-agents, and even Telegram and the
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

`~/.august/extensions/ask-me/extension.json`:

```json
{"command": ["python3", "main.py"]}
```

`main.py`:

```python
import json, sys, threading, itertools, queue

lock, ids, waiting = threading.Lock(), itertools.count(), {}

def send(msg):
    with lock:
        print(json.dumps(msg), flush=True)

def call(method, **params):  # call August and wait for the reply
    id, reply = f"q{next(ids)}", queue.Queue()
    waiting[id] = reply
    send({"id": id, "method": method, "params": params})
    return reply.get().get("result")

def tool_call(id, data):
    verdict = None
    if data["tool"] == "bash":
        listener = call("listen", thread="home", buttons=["run", "block"])
        call("send", thread="home", message={
            "text": f"Run `{data['input'].get('command')}`?",
            "buttons": [{"id": "run", "label": "Run"}, {"id": "block", "label": "Block"}]})
        if (call("next", listener=listener) or {}).get("press") != "run":
            verdict = {"block": "the user said no"}
    send({"id": id, "result": verdict})

send({"method": "ready", "params": {
    "protocol": 2, "summary": "Asks the user before every shell command",
    "events": ["tool_call"], "needs": ["messaging"], "timeouts": {"tool_call": 300000}}})

for line in sys.stdin:
    msg = json.loads(line)
    if "method" not in msg:
        waiting.pop(msg["id"]).put(msg)  # August's reply to one of our calls
    elif msg["method"] == "event":
        threading.Thread(target=tool_call, args=(msg["id"], msg["params"]["data"])).start()
    elif "id" in msg:
        send({"id": msg["id"], "error": {"message": f"unknown method {msg['method']}"}})
```

Run `/reload`, and the next `rm -rf` waits for you on your phone.

**Would you rather have new extensions picked up on their own? Write an extension for that!**

The same protocol covers the rest: tools, slash commands, hooks on every step of a turn
(rewrite the prompt, swap the model, trim the context), sub-agents, and whole new
messengers and model providers. See [PROTOCOL.md](PROTOCOL.md).

Real extensions use an SDK: `august-ext` for Rust (in `sdk/`) or the TypeScript SDK in
august-agent. Or skip writing it yourself and ask the agent to write the extension.

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
