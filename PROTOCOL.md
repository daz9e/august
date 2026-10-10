# The August extension protocol

Version **2**. This is everything August's core knows about extensions, and everything an
extension may rely on. It is written from the core (`src/extensions/`, `src/gateway/`) and
the Rust SDK (`sdk/`); where this file and the code disagree, the code is right and this
file has a bug.

An extension is a process. It talks to August over its stdin and stdout and may be written
in any language. Everything an agent does beyond its loop lives in extensions: tools,
slash commands, messengers (Telegram, the terminal), model providers, sign-ins, how
replies are drawn, approvals, compaction. All of these sit on the same protocol.

Contents:

1. [Transport](#1-transport)
2. [Finding and starting an extension](#2-finding-and-starting-an-extension)
3. [The handshake: `ready` and `manifest`](#3-the-handshake-ready-and-manifest)
4. [Permissions](#4-permissions)
5. [August → extension](#5-august--extension)
6. [Extension → August: operations](#6-extension--august-operations)
7. [Events](#7-events)
8. [Jobs of the core: `render`](#8-jobs-of-the-core-render)
9. [Messengers](#9-messengers)
10. [Model providers](#10-model-providers)
11. [Accounts and sign-in](#11-accounts-and-sign-in)
12. [Supervision](#12-supervision)
13. [Commands of the `august` program](#13-commands-of-the-august-program)
14. [A complete extension](#14-a-complete-extension)

---

## 1. Transport

- **Framing**: one JSON object per line (UTF-8, `\n`-terminated) in each direction. August
  writes to the extension's stdin and reads its stdout.
- **stdout is the protocol.** A line that isn't JSON is copied to the log and otherwise
  ignored. Log to **stderr**: each line goes to `$AUGUST_HOME/logs/extensions/<name>.log`
  and to the `extension_output` event.
- **Messages** look like JSON-RPC without the `jsonrpc` field:

  | Kind | Shape |
  |---|---|
  | Request | `{"id": <id>, "method": "<name>", "params": {...}}` |
  | Reply | `{"id": <id>, "result": <any>}` or `{"id": <id>, "error": {"message": "...", "kind"?: "..."}}` |
  | Notification | `{"method": "<name>", "params": {...}}`, with no `id` and no reply |

- **Both sides send requests.** August calls the extension (an event, a tool) and the
  extension calls August (send a message, start a turn) on the same link, interleaved.
- **Ids**: August numbers its requests 1, 2, 3, … per link. A reply from the extension must
  carry that integer `id`; a reply with an unknown id is dropped. August echoes the `id` of
  the extension's requests back unchanged, so the extension may use any JSON value.
- **Concurrency**: August may send a new request before earlier ones are answered, and
  serves the extension's requests concurrently too. Answer each request on its own task or
  thread; one slow handler must not block the others. Replies may come in any order.
- **Errors**: the `message` of an error reply is shown to whoever asked (the model, for a
  tool). `kind` matters only for model providers (§10). August's own error replies carry
  only `message`.
- **Cancellation**: when August stops waiting for one of its requests (the turn was
  cancelled with `/stop`, or the call timed out), it sends the notification
  `{"method": "cancel", "params": {"id": <id>}}`. Stop working on that request; a reply
  sent after this is ignored. The extension can't cancel its own calls into August.
- **End**: when stdout closes or the process exits, every request still waiting on the
  extension fails with `the extension process exited`.

## 2. Finding and starting an extension

### Where extensions come from

| Kind | Found as | Started as |
|---|---|---|
| User extension | `$AUGUST_HOME/extensions/<name>/extension.json` | the `command` it names, in its folder |
| Default extension | a binary `august-ext-<name>` in the folder `august.defaults` names (`config/august.json`), else next to the `august` binary | that binary |

`$AUGUST_HOME` defaults to `~/.august`. A name is 1–64 characters of `a-z`, `0-9`, `-` and
`_`. A user extension replaces a default one of the same name. (An embedder of the core may
also link extensions in over its own streams; they speak the same protocol.)

### `extension.json`

```json
{"command": ["python3", "main.py"], "env": {"MODE": "fast"}}
```

- `command` (required): a non-empty argv. A relative program with a path in it
  (`./weather`) is resolved against the extension's folder; a bare name (`python3`) is
  looked up on `PATH`.
- `env`: extra environment variables. Every value must be a string.
- The core ignores every other field. The default `setup` extension, for example, reads
  `setup`, a list of commands to run before the start.

### The process

- **Working directory**: the extension's folder. For a default extension this is
  `$AUGUST_HOME/extensions/.runtime/defaults/<name>`.
- **Process group**: the extension leads a group of its own. When it ends, or August stops
  it, the whole group is killed (`SIGKILL`), including anything it started.
- **Environment**: August's environment, plus `env` from `extension.json`, plus:

  | Variable | Set for | Value |
  |---|---|---|
  | `AUGUST_HOME` | all | August's home folder |
  | `AUGUST_WORKSPACE` | all | the agent's workspace, absolute |
  | `AUGUST_EXTENSION_DIR` | defaults | the extension's state folder (its working directory) |
  | `AUGUST_EXTENSIONS` | defaults | `$AUGUST_HOME/extensions` |

### Per-extension settings

Each extension may have a settings file at `config/extensions/<name>.json`. The user owns
it; the core reads these fields:

| Field | Meaning |
|---|---|
| `enabled` | `false`: the extension is not started. Only the user may change it. |
| `early` | `true`: start in the first wave (see below) |
| `origin` | `user` or `agent`: who installed it. Set by the core on first enable and shown in `/extensions`. Only the user may change it. |
| `supervise` | overrides of `august.supervise` for this extension (§12) |
| `settings` | the extension's own settings, as its schema describes them (§3, `settings`) |

### Order of start

At start and on `/reload`, extensions start in two waves. The first wave holds the
defaults, linked extensions and those marked `early`; the rest start after it. That way
`extension_launch` and `extension_ready` hooks (§7.6) are in place before the extensions
they watch. Within a wave, extensions start concurrently.

### Launch sequence

1. August emits `extension_launch` (an admin hook, §7.6). Hooks may change `command` and
   `env`, or block the start.
2. The process starts.
3. Within **30 s** the extension sends `ready` (§3). A different `protocol` is refused
   (`speaks extension protocol N; this August needs 2`). If the process exits first, the
   tail of its stderr is the error.
4. August emits `extension_ready` (an admin hook). Hooks may block the extension or cut its
   `needs`. **Until these hooks pass, the extension gets nothing**: no events, no tool calls,
   no commands. Its own calls into August are served with the permissions it declared.
5. The extension is **running**.

The *reason* of a launch, shown in `/extensions` and passed to the hooks, is one of:

| Reason | When |
|---|---|
| `install` | first seen |
| `update` | its files changed since the last start (judged by size and modification time; hidden folders, `target`, `node_modules` and `__pycache__` are skipped). For a default extension, its binary changed. |
| `start` | August started |
| `reload` | `/reload` |
| `enable` | enabled or saved again |
| `restart` | after a crash |

## 3. The handshake: `ready` and `manifest`

Once started, the extension tells August everything it offers in one notification:

```json
{"method": "ready", "params": {
  "protocol": 2,
  "summary": "Reminders the user sets in chat",
  "details": "Keeps reminders in its store and sends each one to its thread when due.",
  "tools": [{"name": "remind", "description": "Set a reminder", "parameters": {"type": "object", "properties": {"text": {"type": "string"}, "at": {"type": "string"}}, "required": ["text", "at"]}}],
  "commands": [{"name": "reminders", "description": "List reminders"}],
  "events": ["turn_end", "shutdown"],
  "needs": ["messaging"],
  "timeouts": {"tool_call": 120000},
  "sections": [{"name": "reminders", "text": "## Reminders\nUse `remind` when the user asks to be reminded."}],
  "settings": {"type": "object", "properties": {"quiet_hours": {"type": "string", "default": "23-8"}}},
  "emits": [{"name": "due", "description": "A reminder is due", "schema": {"type": "object", "required": ["text"]}, "observe": false}],
  "replaces": [], "takes": [],
  "providers": [], "messengers": [], "accounts": [], "cli": []
}}
```

Every field is optional except `protocol`; a missing `protocol` counts as 1 and is refused.

| Field | Shape | Meaning |
|---|---|---|
| `protocol` | `2` | the protocol version it speaks |
| `summary` | string | one line on what it does; the agent sees it |
| `details` | string | how it works and what it does on its own, so its effects can be traced back to it |
| `tools` | `[{name, description, parameters}]` | tools the model may call; `parameters` is a JSON Schema |
| `commands` | `[{name, description}]` | slash commands (`/name args`) |
| `events` | `[name]` | the events it hooks (§7). August sends `event` only for these. |
| `needs` | `[permission]` | the permissions it uses (§4) |
| `timeouts` | `{event: ms}` | its own timeout per hooked event, overriding the defaults (§5.1) |
| `sections` | `[{name, text}]` | Markdown sections added to the system prompt. Fixed for a conversation once it starts, so a change shows up from the next one. |
| `settings` | JSON Schema | its settings (`config/extensions/<name>.json` → `settings`); properties with `"secret": true` are shown masked; `default`s are filled in by the `settings` operation |
| `emits` | `[{name, description, schema, observe}]` | events it emits, hooked by others as `<name>:<event>` (§7.8) |
| `replaces` | `[extension]` | extensions whose event namespace it takes over (§7.8) |
| `takes` | `[job]` | jobs of the core it does instead (§8); today: `render` |
| `providers` | `[{id, label, default_model}]` | model providers it offers (§10) |
| `messengers` | `[Description]` | messengers it offers (§9) |
| `accounts` | `[{id, label, providers, key, login}]` | accounts it signs in to (§11) |
| `cli` | `[{name, description, exec}]` | commands of the `august` program (§13) |

**`manifest`**: whenever what it offers changes (a tool registered later, a provider that
went away), the extension sends the **whole** object again as
`{"method": "manifest", "params": {...}}`; it replaces the previous one. Turns that start
from then on see the new tools, and messengers get the new command list. A `manifest` sent
*before* `ready` only tells what the extension needs so far, so it can make calls during its
own setup.

**Conflicts**: of two extensions offering a tool or an `august` command of one name, a user
extension wins over a default one (then by name). Of two slash commands of one name, the
first by extension name wins.

## 4. Permissions

Every operation (§6) has a permission or none. An extension may call an operation only if
its `needs` lists that permission; otherwise the call fails with
`` `send` needs the `messaging` permission: declare it with august.needs("messaging") ``.
`extension_ready` hooks may cut the list, and `/extensions` shows it.

| Permission | Grants |
|---|---|
| `messaging` | messengers and any thread: `messengers`, `send`, `edit`, `delete`, `react`, `open_thread`, `action`, `download`, `listen`, `next`, `prompt` |
| `turns` | `turn_start`, `turn_wait`, `turn_cancel`, `turns`, `stop` |
| `tools` | `callTool` |
| `llm` | `llm` |
| `models` | `model_set`, `providers`, `models` |
| `sessions` | `sessions`, `session_new`, `session_update`, `session_switch`, `history`, `messages`, `messages_set`, `search` |
| `config` | `config_list`, `config_get`, `config_set` (any unit's settings) |
| `admin` | `extensions`, `extension_enable`, `extension_disable`, `extensions_reload`, `extension_logs`, `extension_health`, `accounts`, `login`, `logout`; and hooking `extension_launch`, `extension_ready`, `extension_exit`, `extension_output` |
| `user` | `as_user: true` on any call: it counts as the user's (§6) |

Rules:

- **Your own thread is free.** A `messaging` operation needs no permission when its `thread`
  is the thread of a call from August to this extension that is still running (answering
  where you were asked: a tool, a command or a hook in that thread).
- Operations with no permission (`ops`, `tools`, `commands`, `status`, `settings`,
  `settings_set`, `store_*`, `emit`, `journal_append`, `secret_*`, `account_update`,
  `login_*`, `inbound`) only reach the caller's own things.
- Admin hooks are simply not delivered to an extension without `admin`; the hook is not an
  error.

## 5. August → extension

| Method | Params | Reply | Sent to |
|---|---|---|---|
| `event` | `{name, data, ctx}` | changed fields of `data` (§7) | extensions that list `name` in `events`, or take a job (§8) |
| `tool` | `{name, input, ctx}` | the output: a string (anything else is stringified). An error is shown to the model as `error: <message>`. | the extension offering the tool |
| `command` | `{name, args, ctx}` | the reply text, or `null` for none | the extension offering the command |
| `health` | `{}` | `{status: "ok"\|"degraded"\|"failed", detail?}` | every running extension (§12) |
| `cancel` | `{id}` (notification) | — | §1 |
| `account_check`, `login`, `logout` | §11 | §11 | extensions with accounts |
| `complete`, `models` | §10 | §10 | extensions with providers |
| `messenger_*` | §9 | §9 | extensions with messengers |

An extension that doesn't know a method answers with an error. `unknown method <name>` is
the convention, and for `health` any error still counts as alive.

### 5.1 Timeouts

How long August waits for each call. After that it sends `cancel` and goes on: a hook is
skipped, a tool or command fails.

| Call | Timeout |
|---|---|
| `event` (a chain, §7.1) | 10 s, `message_in` 2 min; or the extension's own `timeouts[event]` |
| `event` (an observed one) | 1 h, `stop` 10 s; or `timeouts[event]` |
| `event` `extension_launch` | 15 min |
| `event` `extension_ready`, `extension_exit` | 5 min |
| `event` `shutdown` | 2 s |
| `event` `render` (§8) | 10 s, or `timeouts.render` |
| `tool`, `command` | 10 min |
| `health` | `ping_timeout_ms` (10 s) |
| `account_check`, `login`, `logout` | 15 min |
| `complete`, `models` | 10 min |
| `messenger_*` | 60 s; 5 min for `messenger_send` with files and for `messenger_download` |

August's operations (§6) have no timeout of their own; some take a `timeout_ms`.

### 5.2 `ctx`

`event`, `tool`, `command`, `account_check`, `login` and `logout` carry `ctx`, which says
where the call comes from:

```json
{"thread": {"messenger": "telegram", "id": "123"},
 "turn": {"id": 42, "conversation": "thread", "show": true, "source": "ext:goal", "parent": 7, "meta": {}},
 "depth": 0}
```

- `thread`: the thread the call belongs to, or `null` (e.g. a lifecycle event).
- `turn`: the turn it runs in, or `null` outside a turn.
  - `id`: the turn's id.
  - `conversation`: `thread` (the thread's own), `copy` (a copy of it, dropped afterwards)
    or `new` (one of its own).
  - `show`: whether the turn is shown in the thread as it runs.
  - `source`: who started it. Absent for the user's own turns.
  - `parent`: the turn that started it, if any.
  - `meta`: what the starter attached. The core never looks inside.
  `source`, `parent` and `meta` are left out when empty.
- `depth`: how many extension-emitted events the call is nested in. Pass it back with `emit`
  (§7.8).

When calling August *from inside* such a call, pass `ctx.thread` as `thread`, and
`ctx.turn.id` as `from_turn`. Then the hooks of what you run see the same turn, and
`session_new`/`session_switch` from a thread's own reply wait for that reply to end instead
of cancelling it.

## 6. Extension → August: operations

Every call into August is an **operation** of one table (`src/gateway/ops.rs`). Slash
commands, the agent's tools and `august config` use the same table; `ops` lists it with
permissions. Calls are requests: `{"id", "method": "<operation>", "params": {...}}`.

### 6.1 Common parameters and shapes

- **`thread`**: `{"messenger": "...", "id": "..."}`, or the string `"home"` for the user's
  home thread (the one set with `/home`, else the one the user wrote in last). A stored
  conversation with no chat is the thread `{"messenger": "session", "id": "<session id>"}`.
- **`as_user: true`** (needs `user`): the call counts as the user's. Hooks and the journal
  see the caller as `user` instead of `ext:<name>`, and only the user may turn extensions
  on and off (`config_set` on `enabled` or `origin`).
- **`from_turn`**: the turn the call is made from (§5.2).
- **message**: a string, or
  `{"text": "Markdown", "buttons": [[{"id", "label"}]], "files": ["/abs/path"], "reply_to": "<message id>"}`.
  `buttons` may also be one flat list (a single row). A button's `id` is opaque: a press
  comes back with it. Text is Markdown everywhere; each messenger converts it to what it
  shows, and degrades what it can't show (buttons as a numbered list, files as paths).
- **turn request**:

  | Field | Default | Meaning |
  |---|---|---|
  | `text` | required | the prompt |
  | `conversation` | `thread` | `thread`: the thread's conversation, after whatever runs there now. `copy`: a copy of it, dropped afterwards. `new`: a conversation of its own (a sub-agent). |
  | `show` | `false` | stream it in the thread like a user's turn. It skips `message_in`. |
  | `source` | `ext:<name>` | who starts it (`ctx.turn.source`) |
  | `parent` | — | the turn that asked for it |
  | `system` | — | `new` only: instructions for its system prompt |
  | `tools` | all | only these tools may be called. A `new` conversation is also *offered* only these; the others keep offering everything, so the prompt cache holds. |
  | `exclude` | `[]` | never these tools |
  | `meta` | — | anything for hooks to read (`ctx.turn.meta`) |

- **turn outcome**: `{"status": "ok"|"error"|"cancelled", "reply", "error", "toolCalls": [{name, input, output, isError}]}`.

### 6.2 Reference

**Introspection** (no permission)

| Operation | Params | Result |
|---|---|---|
| `ops` | — | `[{name, permission, about}]` |
| `tools` | — | `[{name, description, parameters, owner}]` |
| `commands` | — | `[{name, description, owner}]` |
| `status` | `thread?` | `{provider, model, workspace, busy}` (`busy`: whether a turn runs in `thread`, `null` without one) |

**Messages** (`messaging`, except in your own thread)

| Operation | Params | Result |
|---|---|---|
| `messengers` | — | `[{id, name, capabilities, notes, actions, threads: [{id, active, last_seen, place}]}]` (§9) |
| `send` | `thread, message` | the new message's id; `""` if a `message_out` hook dropped it. While a reply is being drawn in that thread, the message is placed in order (§8). |
| `edit` | `thread, id, message` | `null` |
| `delete` | `thread, id` | `null` |
| `react` | `thread, id, emoji` | `null` (`""` removes August's reaction) |
| `open_thread` | `thread, title` | the new thread `{messenger, id}` (where `capabilities.open_thread`) |
| `action` | `thread, action, args` | the action's result. `args` is checked against the action's `input_schema` (§9). |
| `download` | `thread, file, path` | `{path, size}`; `file` is an attachment from `message_in`, `path` is relative to the workspace |
| `listen` | `thread, buttons: [id], text: bool, secret: bool, ttl_ms = 600000` | a listener id. From now on it takes the first press of one of `buttons`, or (with `text`) a text message without files, before the agent sees it. With `secret`, the taken message is deleted from the chat. It ends after `ttl_ms` if nobody calls `next`. **Listen before sending the question.** |
| `next` | `listener, timeout_ms = 300000` | `{press: id}`, `{text}`, `{cancelled: "stop"\|"new"}` or `{timeout: true}`; once per listener |
| `prompt` | `thread, text, source = ext:<name>, deliver = "steer"` | `null`. Hands the thread a message as if the user sent it; it passes `message_in`. `deliver`: `steer` joins the running turn before its next model call, or starts one; `followUp` runs as its own turn after the current one; `nextTurn` waits for the next turn without starting one. |

**Turns** (`turns`)

| Operation | Params | Result |
|---|---|---|
| `turn_start` | `thread, turn` | the turn's id. It is registered at once, so `/stop` cancels it even while it waits for the thread. |
| `turn_wait` | `id, timeout_ms = 3600000` | the outcome, or `{status: "running"}` after the timeout. Once per turn; an outcome nobody collects is dropped after 10 min. |
| `turn_cancel` | `id` | `true` if it was running |
| `turns` | `thread?` | running turns: `ctx.turn` objects with their `thread` |
| `stop` | `thread` | `{cancelled: n}`. Like `/stop`: emits `stop`, ends waits and cancels every turn of the thread. |

**Tools and the model**

| Operation | Permission | Params | Result |
|---|---|---|---|
| `callTool` | `tools` | `thread, name, input, from_turn?` | `{output, isError}`. Runs any agent tool through the `tool_call`/`tool_result` hooks, with caller `ext:<name>`. |
| `llm` | `llm` | `prompt` or `messages` (§10.1); `system`, `effort`, `options`, `thread?` | the reply text. One completion without tools. In a thread, it uses that conversation's model and is counted there (`llm_result`). |
| `model_set` | `models` | `model`: `name` of the active provider, `provider:name`, or `provider:` for its default | `{provider, model}`; passes `model_select` |
| `providers` | `models` | — | `[{id, label, default_model, extension}]` |
| `models` | `models` | `provider?` (default: the active one) | `[{id, context_window}]` |

**Conversations** (`sessions`)

| Operation | Params | Result |
|---|---|---|
| `sessions` | `thread?` | `[{id, chat, name, settings, created_at, messages, bound}]`, newest first |
| `session_new` | `thread?, name?, settings?, from_turn?` | the new session's id. With a thread it passes `session_before_new` and becomes that thread's conversation. Without one, it is a conversation of no chat (thread `{messenger: "session", id}`). |
| `session_switch` | `thread, session, from_turn?` | `session`; passes `session_before_switch` |
| `session_update` | `session, name?, settings?` | `null`; `settings` is `{model, system, tools}`, and a `null` field deletes it |
| `history` | `session?` or `thread?`, `kinds?: [kind]`, `since?: entry id`, `limit = 100` | journal entries, oldest first: `[{id, ts, session, turn, kind, source, caller, data}]`. Kinds: `session`, `turn_start`, `user_message`, `assistant`, `tool`, `turn_end`, `model_change`, `custom`. |
| `messages` | `thread` | `{session, messages, tokens, window}`: what the model sees next |
| `messages_set` | `thread, messages` | `null`. Replaces the live conversation; earlier messages stay searchable. |
| `search` | `query, limit = 8 (1–50)` | `[{at, role, text}]`, full text over every conversation |

**Extensions** (`admin`)

| Operation | Params | Result |
|---|---|---|
| `extensions` | — | `[{name, state, reason, error, origin, restarts, final, health?, pid?, uptime_s?, memory_kb?, cpu?, summary?, details?, tools?, commands?, accounts?, hooks?, needs?, takes?, sections?, events?}]`. `state` is `starting`, `running`, `degraded`, `failed` or `disabled`; `final` means it won't be restarted until `/reload`. |
| `extension_enable` | `name` | its status line; fails with its error. (Re)starts it and keeps it enabled. |
| `extension_disable` | `name` | `null`. Stops it (`shutdown` with `disable`) and keeps it disabled. |
| `extensions_reload` | — | like `extensions`. Restarts every extension but the caller. |
| `extension_logs` | `name, lines = 100` | the tail of its log: its stderr and the core's notes, also while it is down |
| `extension_health` | `name` | `{status: ok\|degraded\|failed\|hung, detail}` |
| `accounts` | — | §11 |
| `login` | `account, thread` | §11 |
| `logout` | `account` | §11 |

**Settings**

| Operation | Permission | Params | Result |
|---|---|---|---|
| `settings` | — | — | the caller's settings, with the schema's defaults filled in |
| `settings_set` | — | `path, value` | `null`; `path` is inside the caller's settings, and a `null` value deletes it. Emits `config_changed`. |
| `config_list` | `config` | `prefix?` | `[{path, description, type, default, value, secret}]`, secrets masked |
| `config_get` | `config` | `path` (`august.model`, `extensions.web.settings.key`) | the value, secrets masked as `••••` |
| `config_set` | `config` | `path, value` | `null`; emits `config_changed` |

**Own state** (no permission; each extension sees only its own)

| Operation | Params | Result |
|---|---|---|
| `store_get` | `key` | the stored JSON value, or `null` |
| `store_set` | `key, value` | `null`; `null` deletes |
| `store_list` | `prefix` | `[{key, value}]` in key order |
| `secret_get` | `key` | the string, or `null` |
| `secret_set` | `key, value` | `null`; `null` deletes. Kept in `secrets/<name>.json` (mode 0600). |
| `journal_append` | `type, data, session?` or `thread?` | the entry's id. Recorded as kind `custom`, caller you. |
| `emit` | `event, data, thread?, from_turn?, depth?` | §7.8 |

**Messengers and sign-in** (no permission; only for your own): `inbound` (§9),
`account_update`, `login_ask`, `login_choose`, `login_open`, `login_progress`,
`login_callback`, `login_wait` (§11).

## 7. Events

### 7.1 How events run

August sends `{"method": "event", "params": {"name", "data", "ctx"}}` to every running
extension that lists `name` in its `events` (only to those with `admin` for the four
lifecycle events, §7.6). There are two kinds:

- **Chains** (most events). Extensions run **one after another**: first those in the user's
  order (`hooks.order` in `config/august.json`), then the rest by name. A handler replies
  with an object of only the fields it changes. These fields replace the same top-level
  fields of `data` (a shallow merge), and the next handler sees the result. Replying with
  `null`, a non-object or the unchanged data changes nothing. A reply with
  `block: true | "<reason>"` or `handled: true` **ends the chain**. What the core does with
  the final data is listed per event.
- **Observed** events (marked *observe* below). Every handler gets the event **at once**,
  and replies are ignored.

A handler that fails or times out is skipped, as if it had changed nothing. One extension
gets one `event` request per event; how it splits that between several handlers of its
own is up to its SDK.

### 7.2 Messages

| Event | Data | A handler may return |
|---|---|---|
| `message_in` | `{id?, text, files, source, deliver, steer, addressed, reply_to?, place?}` | `text`; `images: [{path, mime}]` to show the model images (such a message starts its own turn); `deliver`; `addressed: true` to answer a message not meant for August; `handled: true, reply?` to swallow it (sending `reply`) |
| `message_out` | `{kind: "send"\|"edit", id, text, buttons, files, reply_to}` | changed fields, or `block` to drop the message |
| `reaction` *(observe)* | `{message, emoji}` | — |
| `message_edited` *(observe)* | `{id, text}` | — |

- `message_in` runs before the agent sees a message. That includes the user's (`source:
  "user"`, with `id`, `reply_to: {id, text, mine}`, `place`) and a `prompt` (`source` is
  its source).
  - `files`: the attachments as the messenger has them, for `download`.
  - `steer`: the message would join the turn running now.
  - `addressed: false`: in a group, the message isn't meant for August. Nothing runs unless
    a hook sets `addressed: true`.
  - With no text and no images left after the hooks, nothing runs.
- `message_out` runs on every message August sends or edits: replies, command answers,
  questions, and extensions' `send`. A streamed reply passes once per edit. Deletes and
  reactions don't pass it.

### 7.3 A turn

All of these carry `ctx.turn`.

| Event | Data | A handler may return |
|---|---|---|
| `session_start` | `{session, previous, reason: "start"\|"new", chat}` | `model`, `system`, `tools`: the conversation's own settings |
| `before_turn` | `{text, system}` | `text` (the user message), `system` (the base system prompt for this turn) |
| `llm_call` | `{step, system, model, tools: [name]}` | for this call only: `system`; `tools` (only these are offered); `model` (`name` or `provider:name`); `effort`; `options` (fields merged into the provider's request body) |
| `context` | `{step, messages, system, tokens, window, error}` | `messages` (what the model sees in this call only), or `history` with `note` (replaces the conversation from now on and shows `note`) |
| `llm_error` | `{step, attempt, model, error: {kind, message}, streamed}` | `retry: true`, with `delayMs` and `model` (for the rest of the turn) |
| `llm_result` *(observe)* | `{step, session, text, toolCalls: [{name, input}], usage: {inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens}}` | — |
| `tool_call` | `{tool, input, id, caller}` | `input` (changed arguments), `block` (the model sees `blocked by an extension: <reason>`) |
| `tool_result` | `{tool, input, id, caller, output, isError}` | `output`, `isError` |
| `turn_event` *(observe)* | `{kind: "text"\|"step"\|"tool"\|"note", text?, tool?, input?}` | — |
| `turn_end` *(observe)* | `{text, reply, status, error, toolCalls: n, unattended}` | — |
| `turn_settled` *(observe)* | `{}` | — |
| `stop` *(observe)* | `{turns: [id]}` | — |

- `session_start` comes before a conversation's first turn. `start` means the thread's
  first conversation, or after a restart with none; `new` means after `/new`.
- `before_turn` comes once per turn. The core's own base prompt is empty; extensions write
  it.
- `llm_call` comes before every model call; `step` counts from 0. The core sets **no limit
  on steps**: a turn ends when the model stops calling tools. A limit is a handler that
  takes the tools away.
- `context` comes before every model call, after `llm_call`.
  - `messages` uses the shape of §10.1.
  - `tokens` is the input the provider reported last (0 before the first call).
  - `window` is the model's context window, or `null`.
  - `error` is set when the previous try of this call failed and a handler asked for a
    retry.
  - Keep `tool_use` and `tool_result` pairs intact.
- `llm_error` comes when a model call failed. `kind` is listed in §10.3; `streamed` means
  part of the reply was already shown. The core doesn't count tries: stop returning
  `retry` when you've had enough (`attempt` counts from 1).
- `llm_result` comes after every model call: a turn's, and `llm` operations (`step:
  null`).
- `tool_call` and `tool_result` wrap every tool call. `id` is the model's call id (`null`
  for `callTool`); `caller` is `model` or `ext:<name>`.
- `turn_event` reports what a *shown* turn does, in order; text that piles up arrives
  merged.
- `turn_end` comes after any turn. `unattended` means the turn wasn't shown.
- `turn_settled` comes once nothing runs in the thread and nothing is about to: every turn
  ended, no message waits, and `turn_end` handlers are done. It fires once per quiet
  period.
- `stop` comes when the user sent `/stop`, before its turns are cancelled.

### 7.4 Conversations and the model

| Event | Data | A handler may return |
|---|---|---|
| `session_before_new`, `session_before_switch` | `{session, to, by}` (`by`: `user` or `ext:<name>`) | `block` |
| `session_changed` *(observe)* | `{reason: "new"\|"switch", previous, session}` | — |
| `model_select` | `{model, previous}` | `model` (switch to another), `block` |

### 7.5 Settings

| Event | Data |
|---|---|
| `config_changed` *(observe)* | `{path}`, after any setting changed through August |

### 7.6 Lives of extensions

| Event | Who gets it | Data | A handler may return |
|---|---|---|---|
| `extension_launch` | `admin` | `{name, dir, command, env, reason, changed: [path]}` | `command`, `env` (start it otherwise: a sandbox, secrets in the environment); `block` |
| `extension_ready` | `admin` | `{name, reason, changed, manifest}` | `block`; `needs` (the most it may have) |
| `extension_exit` | `admin` | `{name, reason: "exit"\|"hang"\|"unhealthy"\|"start", code, error, restarts, uptime_ms}` | `restart: false` (keep it down), `delay_ms` |
| `extension_output` *(observe)* | `admin` | `{name, line}`, every line of its stderr | — |
| `extension_state` *(observe)* | anyone | `{name, state, reason, error, detail, restarts, final}` | — |
| `shutdown` | the extension itself | `{reason: "reload"\|"disable"\|"stop"\|"restart"\|"signal"}` | — (2 s to clean up, then its process group is killed) |

- `extension_launch` comes before the process starts. `reason` is listed in §2;
  `changed` lists the files that changed.
- `extension_ready` comes once the extension started and registered, before it gets
  anything. `manifest` is a summary: `tools`, `commands`, `hooks`, `needs`, `settings`,
  `emits`, `replaces`, `takes`, and the ids of its `accounts`, `providers`, `messengers`
  and `cli`.
- `extension_exit` comes when its process ended. `exit` means it ended by itself; `hang`
  and `unhealthy` are §12; `start` means a restart failed.
- `extension_state` comes whenever an extension's state changes.
- `shutdown` comes before August stops the extension.

### 7.7 What `ctx` holds

Message and conversation events carry `ctx.thread`. Turn events carry `ctx.turn` too.
Lifecycle and settings events carry neither.

### 7.8 An extension's own events

An extension declares events in `emits` and runs them with the `emit` operation; others
hook them as `<namespace>:<event>`, where the namespace is the extension's name.

```json
{"id": 9, "method": "emit", "params": {"event": "due", "data": {"text": "Call mom"}, "thread": {"messenger": "telegram", "id": "123"}, "from_turn": 42, "depth": 0}}
```

- `event` is a declared name, or `<namespace>:<name>` for a namespace the extension
  `replaces`. Any other namespace is refused: `` events `x:*` belong to x ``.
- `data` must be an object matching the declared `schema`. The top-level `required` keys
  and property `type`s are checked; nested ones are not.
- A chain event (`observe: false`) replies with the data its handlers leave: read `block`
  and the fields you allow them to change. An observed one replies with `data` at once and
  runs in the background.
- `thread` and `from_turn` become the handlers' `ctx`. Pass `depth` from your `ctx`:
  events may nest at most 8 deep, which stops handlers emitting each other in a loop.
- **`replaces`**: an extension that stands in for another (your own `compaction`) declares
  `replaces: ["compaction"]` and emits `compaction:<event>`, so existing hooks keep
  working. Its events are listed under that namespace.

Emit whether anyone listens or not. Hook another extension's event only for extra
behaviour: nothing arrives while it isn't running.

## 8. Jobs of the core: `render`

Some things the core does only while no extension does them in its place. An extension
takes a job with `takes`; of several, the first in `hooks.order` gets it. Today there is
one job, `render`: drawing the user's **shown** turns in their thread.

The taker gets `event` requests named `render` (whether or not it lists `render` in
`events`) with `ctx.thread` and `ctx.turn`. They come **one at a time, in order**: the next
waits for the reply, and text that arrives meanwhile comes merged. The taker sends and edits
messages in `ctx.thread` itself.

| `data.kind` | Data | Meaning |
|---|---|---|
| `start` | `{capabilities}` | a turn begins; the messenger's capabilities (§9) |
| `text` | `{text}` | a fragment of the reply |
| `step` | — | a new model call starts after tool results |
| `tool` | `{tool, input}` | a tool call |
| `note` | `{text}` | a line a `context` handler asked to show |
| `break` | — | another message (a file, a question) goes out in the reply's place: finish what is shown and continue in a new message below |
| `end` | `{status, reply, error}` | the outcome |

When anyone else `send`s to that thread mid-reply, August hands the renderer a `break`,
waits for its reply, and only then sends. With nobody taking `render`, the core sends each
turn's outcome once: the reply, `⏹ Stopped.` or `⚠️ Error: …`.

## 9. Messengers

A messenger connects August to people (Telegram, a terminal window). An extension offers one
by listing its **Description** in `messengers`:

```json
{"id": "telegram", "name": "Telegram",
 "capabilities": {"markdown": true, "max_len": 4096, "buttons": 100, "edit": true, "edit_interval_ms": 1000,
   "files_in": true, "files_out": true, "images": true, "audio_in": true, "commands": true, "presence": true,
   "delete": true, "reactions": true, "reply": true, "threads": true, "open_thread": true},
 "notes": "Buttons show under the message. Uploads up to 50 MB.",
 "actions": [{"name": "pin", "description": "Pin a message", "input_schema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}}]}
```

- **capabilities**: what it can do; whatever it leaves out (false, 0), it can't. The core
  and other extensions decide by these instead of assuming anything about the messenger.

  | Field | Meaning |
  |---|---|
  | `markdown` | Markdown is rendered |
  | `max_len` | longest message, in Markdown characters |
  | `buttons` | buttons under one message, at most |
  | `edit` / `edit_interval_ms` | sent messages can be edited / shortest gap between two edits |
  | `files_in` / `files_out` | files can come in / go out |
  | `images` | images are shown inline |
  | `audio_in` | voice and audio can come in |
  | `commands` | a menu of `/commands` |
  | `presence` | shows that August is busy |
  | `delete` | sent messages can be deleted |
  | `reactions` | emoji reactions |
  | `reply` | a message can answer another one |
  | `threads` | more than one thread |
  | `open_thread` | August can open a thread inside a place |

- **notes**: prose for the agent and the user.
- **actions**: what only this messenger can do, described like tools. They are called with
  the `action` operation, after the core checked the arguments against `input_schema`.

### 9.1 What comes in: `inbound`

The extension hands August whatever comes in from its threads with the `inbound` operation.
It only works for a messenger the extension itself offers.

```json
{"id": 3, "method": "inbound", "params": {
  "thread": {"messenger": "telegram", "id": "123"},
  "user": {"id": "55", "name": "Pavel"},
  "place": {"kind": "group", "parent": null, "title": "Family"},
  "kind": "message", "id": "901", "text": "hi @august", "files": [], "reply_to": null, "addressed": true}}
```

- `user` is required. `place` defaults to a private chat: `kind` is `dm`, `group`,
  `channel` or `thread` (a topic inside `parent`).
- The kind's own fields sit beside `kind`:

  | `kind` | Fields |
  |---|---|
  | `message` | `id`, `text`, `files: [Attachment]`, `reply_to: {id, text, mine}?`, `addressed` (default `true`) |
  | `edited` | `id`, `text` |
  | `reaction` | `message`, `emoji` (`""`: taken back) |
  | `command` | `name` (lowercase, without `/`), `args` |
  | `press` | `button` (the button's `id`) |

- An attachment is `{id, kind: "voice"|"audio"|"image"|"video"|"document", name?, mime?, size?}`.
  The `id` is the messenger's own handle, used by `messenger_download`.
- **`addressed`** is a fact the messenger reports: whether the message is meant for August
  (always in a private chat; in a group when August is mentioned or replied to). The core
  decides what to do with it.
- The reply is `null` at once; the message is handled in the background. Waits (`listen`)
  take what they wait for first; then commands run, and messages go through `message_in` to
  the agent.

### 9.2 What August asks of a messenger

Each call carries `messenger` (the messenger's id). `thread` is the thread's id within that
messenger (a string, not an object).

| Method | Params | Reply |
|---|---|---|
| `messenger_send` | `messenger, thread, message: {text, buttons: [[Button]], files, reply_to}` | the new message's id |
| `messenger_edit` | `messenger, thread, id, message` | `null` |
| `messenger_delete` | `messenger, thread, id` | `null` |
| `messenger_react` | `messenger, thread, id, emoji` | `null` |
| `messenger_presence` | `messenger, thread, busy` | `null`. August starts or stops working there. Show it your way ("typing…") and keep it up until it changes. |
| `messenger_commands` | `messenger, commands: [{name, description}]` | `null`; the command menu, sent whenever it changes |
| `messenger_threads` | `messenger` | `[thread id]` it knows of |
| `messenger_open_thread` | `messenger, parent, title` | the new thread's id |
| `messenger_action` | `messenger, thread, action, args` | its result |
| `messenger_download` | `messenger, file: Attachment` | the bytes, base64 |

## 10. Model providers

An extension offers model providers by listing `{id, label, default_model}` in `providers`.
The user picks one by id (`/login`, `/model`, `provider` in `config/august.json`). August
reaches it through two methods.

### 10.1 `complete`

```json
{"id": 12, "method": "complete", "params": {
  "provider": "anthropic", "model": "claude-opus-5-5", "effort": "medium", "session": "c0ffee",
  "system": "You are August...",
  "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
  "tools": [{"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}],
  "options": null}}
```

- `session`: a stable id per conversation, for routing and prompt caching.
- `effort`: `low`, `medium` or `high`, as the provider takes it.
- `options`: fields to merge into the provider's own request body (`{"temperature": 0}`).
  August never looks inside.
- **Messages** are `{role: "user"|"assistant", content: [Block]}`. A block is one of:

  | `type` | Fields |
  |---|---|
  | `text` | `text` |
  | `tool_use` | `id`, `name`, `input` |
  | `tool_result` | `tool_use_id`, `content` (a string), `is_error` |
  | `image` | `media_type`, `path` (an absolute file path; read and encode it yourself) |
  | `opaque` | `value`: a block of the provider's own (e.g. thinking) that must go back unchanged; other providers skip it |

The reply is the **completion**:

```json
{"message": {"role": "assistant", "content": [{"type": "text", "text": "Hello!"}]},
 "stop_reason": "end_turn",
 "usage": {"input": 812, "output": 6, "cache_read": 0, "cache_write": 0}}
```

`stop_reason` is `end_turn`, `tool_use`, `max_tokens`, `refusal` or anything else. `input`
excludes the cached part, so the four usage counts add up to everything billed.

### 10.2 Streaming

While it works on a `complete`, the provider streams the reply text as notifications. Each
one names the request it belongs to:

```json
{"method": "stream", "params": {"request": 12, "event": {"type": "text", "text": "Hel"}}}
```

Every `stream` notification sent before the reply counts. A provider that doesn't stream
just replies, and August shows the text at the end.

### 10.3 Errors

A failed call replies with an error whose `kind` tells callers what to do; `llm_error`
handlers decide by it.

| `kind` | Meaning |
|---|---|
| `rate_limit` | too many requests; later helps |
| `overloaded` | the service is busy or failed on its side |
| `network` | the connection or the stream broke |
| `auth` | the key or login is missing, wrong or expired |
| `quota` | the plan's or account's limit is used up |
| `context_too_long` | the request doesn't fit the model's window |
| `refused` | the model declined |
| `bad_request` | wrong in a way trying again won't fix |
| `other` | anything else |

### 10.4 `models`

`{"provider": "<id>"}` → `[{"id": "claude-opus-5-5", "context_window": 200000}]`.
`context_window` may be `null`. With no `default_model`, the provider's default is the
first model it lists.

## 11. Accounts and sign-in

An extension declares the accounts it signs in to; August runs every sign-in and keeps the
secrets. The extension only scripts the steps.

```json
"accounts": [
  {"id": "anthropic", "label": "Anthropic", "providers": ["anthropic"], "key": {"label": "API key", "env": "ANTHROPIC_API_KEY"}, "login": false},
  {"id": "chatgpt", "label": "ChatGPT", "providers": ["chatgpt"], "key": null, "login": true}
]
```

`providers` are the model providers the account unlocks; after `/login`, August switches to
the first one.

**Key accounts** (`key` set):

1. On `/login <id>`, August asks the user for `key.label` in the thread, and deletes the
   answer from the chat.
2. It calls `account_check {account, key, ctx}`; an error reply rejects the key.
3. It keeps the key as the extension's secret under the account's id (`secret_get
   {key: "<account id>"}`).
4. The account counts as connected while that secret exists, or while the `key.env`
   variable is set (which then stands in for it).

**Login accounts** (`login: true`):

1. August calls `login {account, session, ctx}` and waits up to 15 min.
2. The extension drives the sign-in with steps that name that `session`. Each step shows in
   the thread the sign-in started from:

   | Operation | Params | Result |
   |---|---|---|
   | `login_ask` | `session, label, secret?` | the user's answer (with `secret`, deleted from the chat) |
   | `login_choose` | `session, question, options: [string]` | the option picked |
   | `login_open` | `session, url, note?` | `null`. Shows the link (opens it in the browser when the user is at this machine's terminal). |
   | `login_progress` | `session, text` | `null` |
   | `login_callback` | `session, port (0: any), path = "/callback"` | the address to redirect to (`http://localhost:<port><path>`); August listens there on 127.0.0.1 for one request |
   | `login_wait` | `session, timeout_ms = 300000` | the redirect's query parameters as an object, or those of an address the user pastes (a browser on another device ends on a page that doesn't load) |

   Each question waits up to 5 min. `/stop` or `/new` cancels the sign-in.
3. The extension replies `{who, expires_at?}`. August records the account as connected by
   `who`. Keep tokens with `secret_set`.

**`logout {account, ctx}`**: forget what you keep. For key accounts August deletes the key
itself.

**`account_update {account, status: "none"|"connected"|"expired", who?}`**: tell August how
your account stands. `expired` tells the user in the home thread to sign in again.

**`accounts`** (`admin`) → `[{id, label, extension, providers, kind: "key"|"login", status, who}]`.
`login {account, thread}` and `logout {account}` (`admin`) run the same flows for another
extension (e.g. the `/login` command).

## 12. Supervision

August watches every running extension. The policy is `august.supervise` in
`config/august.json`, overridden per extension by `supervise` in its settings file:

| Field | Default | Meaning |
|---|---|---|
| `ping_interval_ms` | 30000 | how often August sends `health` |
| `ping_timeout_ms` | 10000 | how long it waits for the reply |
| `ping_misses` | 2 | missed replies in a row → `hang`; `failed` replies in a row → `unhealthy` |
| `max_restarts` | 5 | restarts in a row before it stays down until `/reload` |
| `stable_after_ms` | 600000 | running this long resets the restart count |

- **Health**: any reply means alive. An error reply means alive with no check of its own.
  `degraded` is shown (something outside is wrong; a restart won't help). `failed` is
  something broken inside: after `ping_misses` of them in a row, it is killed as
  `unhealthy`. No reply `ping_misses` times in a row: killed as `hang`.
- **Crash and restart**: an ended process goes through `extension_exit` (§7.6). Unless a
  hook says `restart: false` or the restarts ran out, it starts again after 1, 2, 4, … s,
  at most 60 s (`delay_ms` from a hook overrides the wait).
- **Stop**: before a reload, a disable or August's own stop, an extension that hooks
  `shutdown` gets it and has 2 s. Then its process group is killed.

## 13. Commands of the `august` program

An extension may add commands to the `august` program by listing them in `cli`:

```json
"cli": [{"name": "tui", "description": "Open the terminal chat", "exec": ["/path/to/august-tui"]}]
```

`august <name> args…` runs `exec` with `args` appended, in the user's terminal. `""` is
`august` with no command. The program gets `AUGUST_SOCKET` and `AUGUST_TOKEN`, and may
call August's operations **as the extension that registered the command**, with its
permissions. It does so over the control socket (`$AUGUST_HOME/control.sock`, this user
only), one JSON object per line:

```json
{"token": "<AUGUST_TOKEN>", "call": "send", "params": {"thread": "home", "message": "hi"}}
```

→ `{"result": ...}` or `{"error": "..."}`.

## 14. A complete extension

A tool in Python, in `~/.august/extensions/hello/`:

`extension.json`:

```json
{"command": ["python3", "main.py"]}
```

`main.py`:

```python
import json, sys

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

send({"method": "ready", "params": {
    "protocol": 2,
    "summary": "Greets people",
    "tools": [{"name": "hello", "description": "Greet someone by name",
               "parameters": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}],
}})

for line in sys.stdin:
    msg = json.loads(line)
    if "method" not in msg or "id" not in msg:
        continue  # a reply to a call of ours, or a notification (cancel)
    if msg["method"] == "tool" and msg["params"]["name"] == "hello":
        send({"id": msg["id"], "result": f"Hello, {msg['params']['input']['name']}!"})
    else:
        send({"id": msg["id"], "error": {"message": f"unknown method {msg['method']}"}})
```

This one answers requests one at a time. That is fine for a tool this quick; anything that
calls back into August or takes long must answer concurrently (§1). The Rust SDK (`sdk/`,
crate `august-ext`) and the TypeScript SDK (august-agent's `extensions/sdk-ts`) do all of
this for you. `august-ext`'s `FakeAugust` speaks this protocol from August's side, for
testing an extension without the core.
