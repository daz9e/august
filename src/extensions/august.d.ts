// Types of the August extension API. Use with `import type { August } from "august";`
// (type-only imports are erased when bun runs the file).

declare module "august" {
  /** A conversation in a messenger: a Telegram chat (`{ messenger: "telegram", id: "123" }`),
   *  a terminal window (`{ messenger: "cli", id: "1" }`), ... Where a call takes a thread,
   *  `"home"` is the user's home thread (set with /home; else the one they wrote in last).
   *  `{ messenger: "session", id }` is a stored conversation itself, in no chat: turns that
   *  aren't shown run in it, `sessions.messages` reads it; nothing can be sent there. */
  export type Thread = { messenger: string; id: string } | "home";

  /** A button under a message; a press comes back with its `id`. */
  export type Button = { id: string; label: string };
  /** What to send: Markdown text, optionally with buttons (one row, or rows), local files (the
   *  text is their caption) and the id of a message it answers. Each messenger renders all of
   *  it its own way, degrading what it can't show. */
  export type OutMessage = string | { text: string; buttons?: Button[] | Button[][]; files?: string[]; reply_to?: string };

  /** Where a thread is: a private chat, a group, a channel, or a thread (topic) inside
   *  `parent` (a thread id of the same messenger). */
  export type Place = { kind: "dm" | "group" | "channel" | "thread"; parent: string | null; title: string | null };

  /** What a messenger says about itself. */
  export interface Messenger {
    id: string;
    name: string;
    capabilities: {
      markdown: boolean; max_len: number; buttons: number; edit: boolean; edit_interval_ms: number;
      files_in: boolean; files_out: boolean; images: boolean; audio_in: boolean;
      commands: boolean; presence: boolean; delete: boolean; reactions: boolean; reply: boolean; threads: boolean;
      /** It can open a thread inside one of its threads (`august.openThread`). */
      open_thread: boolean;
    };
    /** What else to know about it, in prose. */
    notes: string;
    /** What this messenger alone can do, described like tools; run with `august.action`. */
    actions: { name: string; description: string; input_schema: object }[];
    /** Threads it knows of; `active` is the one the user wrote in last. */
    threads: { id: string; active: boolean; /** Unix ms of the last message from it. */ last_seen: number | null; place: Place }[];
  }

  /** What a listener took: a button press or a text message; or why nothing came. */
  export type Reply = { press: string } | { text: string } | { timeout: true } | { cancelled: "stop" | "new" };

  /** `source`: who it is from (default `ext:<your name>`; `user` passes it as the user's);
   *  the default `conversation` extension marks it `[from <source>]`. `deliver`: `steer` (default) joins the
   *  running turn before its next model call or starts one; `followUp` runs as its own turn
   *  after the current one; `nextTurn` waits for the next turn without starting one. */
  export type PromptOpts = { source?: string; deliver?: "steer" | "followUp" | "nextTurn" };
  /** Which conversation a turn runs in: `thread`, the thread's own; `copy`, a copy of it (same
   *  prompt and tools, so the provider's cache holds) dropped afterwards; `new`, one of its own
   *  (a sub-agent). */
  export type Conversation = "thread" | "copy" | "new";
  /** A run of the agent. `show`: streamed to its thread as it runs, like the user's turns. */
  export type Turn = { id: number; conversation: Conversation; show: boolean; source?: string; parent?: number; meta?: unknown };
  export type TurnRequest = {
    text: string;
    /** Default `thread`. */
    conversation?: Conversation;
    /** Shown in the thread as it runs, like one of the user's (the default `conversation`
     *  extension marks it `[from <source>]`). Default false: nothing shown, the reply returned. */
    show?: boolean;
    /** Who starts it (default `ext:<your name>`); hooks see it as `ctx.turn.source`. */
    source?: string;
    /** The turn this one belongs to (e.g. `ctx.turn.id`). */
    parent?: number;
    /** new: instructions added to the base system prompt. */
    system?: string;
    /** Only these tools may be called. A `new` conversation is offered only these; the others
     *  keep offering all (so the cache holds), a call to another fails. */
    tools?: string[];
    /** Never these tools (like `tools`). */
    exclude?: string[];
    /** Anything for hooks to read as `ctx.turn.meta` (e.g. `{ approve: "all" }` for the
     *  default approvals extension); the core doesn't look inside. */
    meta?: unknown;
  };
  export type TurnOutcome = {
    status: "ok" | "error" | "cancelled" | "running";
    reply: string;
    error?: string | null;
    /** Every tool call of the turn. */
    toolCalls: { name: string; input: any; output: string; isError: boolean }[];
  };

  /** What an extension declares it uses (`august.needs`); each operation needs at most one. */
  export type Permission = "messaging" | "turns" | "tools" | "llm" | "models" | "sessions" | "config" | "admin" | "user";

  /** An operation of the core's table. */
  export type Op = { name: string; permission: Permission | null; about: string };

  /** A conversation's own settings. `model`: a model of the active provider or
   *  `provider:model`; `system`: instructions added to the system prompt; `tools`: only these. */
  export type SessionSettings = { model?: string; system?: string; tools?: string[] };
  export type SessionInfo = {
    id: string;
    /** The chat (`messenger:id`) that started it. */
    chat: string;
    name: string | null;
    settings: SessionSettings;
    created_at: number;
    messages: number;
    bound: string[];
  };

  /** What the journal records. `source`: who the turn came from (`user`, `ext:goal`, ...);
   *  `caller`: who acted (`model`, `ext:<name>`, `user`). `data` by kind:
   *  `turn_start` {conversation, show, parent, text}, `turn_end` {status, error}, `user_message` {text},
   *  `assistant` {step, text, toolCalls, usage}, `tool` {tool, id, input, output, isError},
   *  `session` {reason: new|switch, previous},
   *  `model_change` {model, previous}, `custom` {type, data} (an extension's own). */
  export type JournalKind = "turn_start" | "turn_end" | "user_message" | "assistant" | "tool" | "session" | "model_change" | "custom";
  export type JournalEntry = {
    id: number;
    /** Unix milliseconds. */
    ts: number;
    session: string | null;
    turn: number | null;
    kind: JournalKind;
    source: string | null;
    caller: string | null;
    data: any;
  };


  export type ExtensionInfo = {
    name: string;
    state: "running" | "failed" | "disabled";
    error: string | null;
    /** Who installed it: shipped with August, the user, or the agent (`save_extension`). */
    origin: "default" | "user" | "agent";
    /** Only while running: what it says it does (`describe`). */
    summary?: string; details?: string;
    /** Only while running: what it registered. */
    tools?: string[]; replaces?: string[]; commands?: string[]; hooks?: string[]; needs?: Permission[]; takes?: string[]; sections?: string[];
    /** Events it emits, as others hook them. */
    events?: { name: string; description: string; schema: object | null; observe: boolean }[];
  };

  export interface Context {
    /** The thread the event, tool call or command belongs to (null outside one). */
    thread: Thread | null;
    /** The turn it runs in (null outside one). */
    turn: Turn | null;
    /** Aborted when August stops waiting for this call (its turn was cancelled, or it timed
     *  out); pass it to fetch and the like to stop work nobody needs. */
    signal: AbortSignal;
    /** Sends a message to that thread; resolves to its id (for `august.edit`). */
    send(message: OutMessage): Promise<string>;
    /** Hands the thread a message (see `PromptOpts`); passes `message_in`. */
    prompt(text: string, opts?: PromptOpts): Promise<void>;
    /** Runs a sub-agent for this thread (a turn in a `new` conversation under this one),
     *  nobody to answer questions. Resolves to its final reply; throws
     *  if it failed or was cancelled (/stop cancels all of a thread's turns).
     *  `system` is added to the base system prompt; `tools` limits it to those tools,
     *  `exclude` hides some. */
    agent(task: string, opts?: { system?: string; tools?: string[]; exclude?: string[] }): Promise<string>;
    /** `august.ask` in this thread. */
    ask(question: string, options: string[], opts?: { timeout?: number }): Promise<string | null>;
    /** Runs any agent tool (MCP or extension) for this thread, with its hooks. */
    callTool(name: string, input?: object): Promise<{ output: string; isError: boolean }>;
    /** One completion without tools on this thread's conversation's model (counted in its
     *  usage); a prompt, or messages as the `context` hook has them. Returns the text. */
    llm(prompt: string | Message[], opts?: { system?: string; effort?: string; options?: Record<string, unknown> }): Promise<string>;
    /** `august.emit` for this thread and turn (their handlers see both). */
    emit<T extends object = any>(event: string, data?: object): Promise<T>;
  }

  export type Block =
    | { type: "text"; text: string }
    | { type: "tool_use"; id: string; name: string; input: any }
    | { type: "tool_result"; tool_use_id: string; content: string; is_error: boolean }
    | { type: "image"; media_type: string; path: string }
    | { type: "opaque"; value: any };
  export type Message = { role: "user" | "assistant"; content: Block[] };

  /** How a model call failed. */
  export type LlmErrorKind = "rate_limit" | "overloaded" | "network" | "auth" | "quota" | "context_too_long" | "refused" | "bad_request" | "other";

  export type Usage = { inputTokens: number; outputTokens: number; cacheReadTokens: number; cacheWriteTokens: number };

  /** What each event handler receives. */
  export interface Events {
    /** A message arrived (before the agent sees it). `files`: its attachments as the
     *  messenger has them, for `august.download`; the default `attachments` extension saves
     *  them into `workspace/inbox` and leaves `{ path, mime, kind, voice }` for later hooks. */
    message_in: {
      /** The message's id in its messenger (for `react`, `reply_to`); null for a `prompt`. */
      id: string | null;
      text: string;
      files: any[];
      /** `user` for what came from a messenger, else the `source` of a `prompt`. */
      source: string;
      /** How it reaches the agent (see `PromptOpts`). */
      deliver: "steer" | "followUp" | "nextTurn";
      /** It would join the turn running now instead of starting one. */
      steer: boolean;
      /** Meant for August (false: e.g. group chatter without a mention). Nothing runs for
       *  a message left unaddressed; set it to true to have the agent answer. */
      addressed: boolean;
      /** The message it answers (`conversation` quotes it for the agent). `mine`: August sent it. */
      reply_to?: { id: string; text: string; mine: boolean } | null;
      /** Where it was written (messages from a messenger). */
      place?: Place;
    };
    /** A turn is about to start; `system` is the base system prompt (empty unless a
     *  sub-agent's; the `conversation` extension writes August's). */
    before_turn: { text: string; system: string };
    /** August is about to send (`kind: "send"`) or replace (`"edit"`, message `id`) a message
     *  in `ctx.thread`, whoever sends it; streamed replies pass once per edit. */
    message_out: {
      kind: "send" | "edit";
      id: string | null;
      text: string;
      buttons: { id: string; label: string }[][];
      files: string[];
      reply_to: string | null;
    };
    /** A tool is about to run. `id`: the model's call id (null outside a model call);
     *  `caller`: `model`, or `ext:<name>` for an extension's callTool. */
    tool_call: { tool: string; input: any; id: string | null; caller: string };
    /** A tool finished. */
    tool_result: { tool: string; input: any; id: string | null; caller: string; output: string; isError: boolean };
    /** Any turn finished (observe only; runs in the background). */
    turn_end: {
      text: string;
      reply: string;
      status: "ok" | "error" | "cancelled";
      /** Why it failed (`status: "error"`). */
      error: string | null;
      /** How many tool calls the turn made. */
      toolCalls: number;
      /** Not shown in the thread (`ctx.turn` says more). */
      unattended: boolean;
    };
    /** What a visible turn does, in order, as it happens: `text` (reply fragments; ones that
     *  arrive while handlers run come merged), `step` (a new model call after tool results),
     *  `tool` (a call starts), `note` (a line a `context` handler asked to show). Observe
     *  only; background. */
    turn_event:
      | { kind: "text"; text: string }
      | { kind: "step" }
      | { kind: "tool"; tool: string; input: any }
      | { kind: "note"; text: string };
    /** Only for the extension that `takes("render")`: a visible turn to draw, one event at a
     *  time (the next waits for the handler; text arriving meanwhile comes merged). `start`
     *  carries the messenger's capabilities (`edit`, `edit_interval_ms`, `max_len`, ...);
     *  `break`: a message was sent to the thread in the reply's place, so finish what is shown
     *  and continue in a new message below; `end`: the outcome. */
    render:
      | { kind: "start"; capabilities: { edit: boolean; edit_interval_ms: number; max_len: number; [k: string]: any } }
      | { kind: "text"; text: string }
      | { kind: "step" }
      | { kind: "tool"; tool: string; input: any }
      | { kind: "note"; text: string }
      | { kind: "break" }
      | { kind: "end"; status: "ok" | "error" | "cancelled"; reply: string; error: string | null };
    /** Nothing runs in `ctx.thread` any more and nothing is about to: every turn ended, no
     *  message waits, and the `turn_end` handlers (which may start the next turn) are done.
     *  Once per quiet period. Observe only; background. */
    turn_settled: {};
    /** The user reacted to `message` with `emoji` (empty: took it back). Observe only. */
    reaction: { message: string; emoji: string };
    /** The user changed the text of their message `id`. Observe only. */
    message_edited: { id: string; text: string };
    /** An extension started, failed, crashed or was turned off (observe only; background).
     *  `error` says why it failed or crashed (the last lines it wrote to stderr). */
    extension_state: { name: string; state: "running" | "failed" | "disabled"; error: string | null };
    /** A setting changed (`/config`, `august.config.set`, `august.settings.set`); `path` is
     *  like `extensions.web.settings.timeout`. Observe only; background. */
    config_changed: { path: string };
    /** August is about to stop this extension (reload, disable): clean up, within 2 s. */
    shutdown: {};
    /** The user sent /stop in the thread (observe only). */
    stop: { turns: number[] };
    /** The user or an extension is switching the model (`/model`, `model_set`). */
    model_select: { model: string; previous: string };
    /** Before each model call of a turn; `step` counts from 0, `system` is the full prompt. */
    llm_call: { step: number; system: string; model: string; tools: string[] };
    /** Before each model call, after `llm_call`: the conversation the model is about to see,
     *  with `system`, the input tokens the provider reported last (`tokens`, 0: not yet) and
     *  the model's context `window` (null: unknown). `error`: why the previous try of this
     *  call failed, when an `llm_error` handler asked for another. */
    context: {
      step: number;
      messages: Message[];
      system: string;
      tokens: number;
      window: number | null;
      error: { kind: LlmErrorKind; message: string } | null;
    };
    /** A model call failed; `attempt` counts from 1. Return `retry: true` to try again.
     *  `streamed`: part of the failed reply was already shown; another try repeats it. */
    llm_error: { step: number; attempt: number; model: string; error: { kind: LlmErrorKind; message: string }; streamed: boolean };
    /** After each model call August makes (observe only; background): a turn's
     *  (`step`), or a single `llm` call (`step` null). `session`: the conversation it counts
     *  for (null: none). */
    llm_result: { step: number | null; session: string | null; text: string; toolCalls: { name: string; input: any }[]; usage: Usage };
    /** Before a conversation's first turn (`start`: the thread's first; `new`: after /new). */
    session_start: { session: string; previous: string | null; reason: "start" | "new"; chat: string };
    /** The thread's conversation was replaced (/new, a switch), right away. Observe only. */
    session_changed: { reason: "new" | "switch"; previous: string; session: string };
    /** /new or `sessions.new` is about to start a conversation in `ctx.thread`. */
    session_before_new: { session: string | null; to: null; by: string };
    /** `sessions.switch` is about to continue conversation `to` in `ctx.thread`. */
    session_before_switch: { session: string | null; to: string; by: string };
  }

  /** What a handler may return; returned fields replace the event's data. */
  export interface Results {
    /** `handled: true` swallows the message (optionally answering with `reply`). */
    /** `images` are shown to the model (a message with images starts its own turn);
     *  `deliver` changes how it reaches the agent. */
    message_in: { text?: string; handled?: boolean; reply?: string; files?: any[]; images?: { path: string; mime: string }[]; deliver?: "steer" | "followUp" | "nextTurn"; steer?: boolean; addressed?: boolean };
    before_turn: { text?: string; system?: string };
    /** Changed fields replace the message's; `block: true` drops it. */
    message_out: { text?: string; buttons?: { id: string; label: string }[][]; files?: string[]; block?: boolean };
    /** `block` (a reason) stops the call; the model sees the reason as an error. */
    tool_call: { input?: any; block?: string };
    tool_result: { output?: string; isError?: boolean };
    turn_end: void;
    turn_settled: void;
    turn_event: void;
    render: void;
    extension_state: void;
    config_changed: void;
    stop: void;
    reaction: void;
    message_edited: void;
    shutdown: void;
    /** `model` switches to another one instead; `block` (a reason) refuses the switch. */
    model_select: { model?: string; block?: string };
    /** `system` replaces the system prompt for this one call (breaks the prompt cache). */
    llm_call: { system?: string; model?: string; tools?: string[] };
    /** Returned `messages` replace what the model sees for this one call; `history` replaces
     *  the conversation itself, stored and kept from then on (e.g. older messages summarised),
     *  with `note` shown in the turn. Keep tool_use / tool_result pairs intact. */
    context: { messages?: Message[]; history?: Message[]; note?: string };
    /** `retry: true` tries the call again (after `delayMs`, on `model` for the rest of the
     *  turn); the `context` handlers see the `error` first. The core doesn't count tries. */
    llm_error: { retry?: boolean; delayMs?: number; model?: string };
    llm_result: void;
    /** The conversation's own settings, kept with it. */
    session_start: SessionSettings;
    session_changed: void;
    session_before_new: { block?: string };
    session_before_switch: { block?: string };
  }

  export interface Tool<P = any> {
    /** Letters, digits, `_` and `-`; a default extension's tool of that name is replaced. */
    name: string;
    /** Tells the model what the tool does and when to use it. */
    description: string;
    /** JSON Schema of the input object. */
    parameters?: object;
    /** Returns the output for the model (non-strings are JSON-encoded); throw to report an error. */
    execute(params: P, ctx: Context): unknown | Promise<unknown>;
  }

  export interface Command {
    description?: string;
    /** `args` is the text after `/name`; a returned string is sent as the reply. */
    handler(args: string, ctx: Context): string | void | Promise<string | void>;
  }

  export interface August {
    /** Extension name (its folder name). */
    name: string;
    /** Extension folder; keep state files here. */
    dir: string;
    /** The agent's workspace folder (absolute); keep the files the agent works with inside it. */
    workspace: string;
    /** `opts.timeout`: how long August waits for this hook (ms; default 10 s, `message_in`
     *  2 minutes), e.g. longer for a `tool_call` hook that asks the user. */
    on<E extends keyof Events>(
      event: E,
      handler: (data: Events[E], ctx: Context) => Results[E] | void | Promise<Results[E] | void>,
      opts?: { timeout?: number },
    ): void;
    /** Another extension's event, `<namespace>:<name>` (see `defineEvent`; `extensions.list()`
     *  shows each one's `events` with their schemas). Nothing arrives while it isn't running. */
    on(
      event: `${string}:${string}`,
      handler: (data: any, ctx: Context) => object | void | Promise<object | void>,
      opts?: { timeout?: number },
    ): void;
    /** Declares an event this extension emits, so others can hook it as `<name>:<event>`
     *  (`<name>` is this extension's). `schema`: JSON Schema of the data (top-level `required`
     *  and property `type`s are checked on emit). `observe`: handlers only watch (run at once,
     *  in the background); otherwise they form a chain in the user's order and may change the
     *  data or `block`. */
    defineEvent(event: string, spec?: { description?: string; schema?: object; observe?: boolean }): void;
    /** Takes over the namespace of extensions this one stands in for (e.g. your own
     *  `compaction`): its events go out as `compaction:<event>`, and the original can't emit
     *  them any more. Emit them as `compaction:<event>`. */
    replaces(...extensions: string[]): void;
    /** Does jobs of the core in its place; the first extension (in `hooks.order`) that takes
     *  one gets it. `render`: draw the user's visible turns from `render` events (sending and
     *  editing messages in `ctx.thread`); with nobody taking it, only each turn's outcome is sent. */
    takes(...jobs: "render"[]): void;
    /** Runs a declared event through its handlers; resolves to the data they leave (an
     *  observed event: to `data`, at once). Inside a handler use `ctx.emit`, so loops are caught. */
    emit<T extends object = any>(event: string, data?: object): Promise<T>;
    /** May be called any time; tools added or removed after setup show up from the next model call. */
    registerTool<P = any>(tool: Tool<P>): void;
    unregisterTool(name: string): void;
    /** Declares what this extension uses beyond its own thread (shown in /extensions; see
     *  the guide for each permission). Without it, those calls fail; answering in the thread
     *  of the call in progress, and the store need nothing. Calls made in setup after it work. */
    needs(...permissions: Permission[]): void;
    /** How this extension is doing, asked every so often (`degraded`: something outside is
     * wrong, restarting won't help; `failed`: broken inside, August restarts it). */
    health(check: () => Health | Promise<Health>): void;
    /** What this extension does. `summary`: one line, shown to the model every turn.
     *  `details`: how it works, what it changes and when (timers, messages it sends, what
     *  it blocks), so the agent can explain or debug it later. Required by `save_extension`. */
    describe(summary: string, details: string): void;
    /** A section of the system prompt (Markdown, e.g. "## Reminders\n..."): how and when the
     *  model should use what this extension offers. Fixed for each conversation, so a change
     *  shows up from the next one (`/new`). Registering a name again replaces it. */
    registerPromptSection(name: string, text: string): void;
    /** `/name` in Telegram and the terminal. */
    registerCommand(name: string, command: Command | Command["handler"]): void;
    /** Something the user signs in to through August (`/login`, which also switches to the
     *  first of its `providers`). August runs the sign-in where the user started it. */
    registerAccount(account: KeyAccount | LoginAccount): void;
    unregisterAccount(id: string): void;
    /** How an account stands; `expired` tells the user to sign in again. */
    accountUpdate(id: string, status: "connected" | "expired" | "none", who?: string): Promise<void>;
    /** This extension's secrets, kept by August in a file only it reads (an API key account's
     *  key is under the account's id). Never keep keys in code, settings or the store. */
    secrets: {
      get(key: string): Promise<string | null>;
      set(key: string, value: string | null): Promise<void>;
    };
    /** This extension's storage in August's database: JSON values by key, kept across
     *  restarts and reloads (only this extension sees them). */
    store: {
      get<T = any>(key: string): Promise<T | null>;
      set(key: string, value: unknown): Promise<void>;
      delete(key: string): Promise<void>;
      /** Entries whose key starts with `prefix`, in key order. */
      list<T = any>(prefix?: string): Promise<{ key: string; value: T }[]>;
    };
    /** Every messenger with its description and threads. */
    messengers(): Promise<Messenger[]>;
    /** Sends a message to any thread; resolves to its id. */
    send(thread: Thread, message: OutMessage): Promise<string>;
    /** Replaces a sent message (where the messenger can edit). */
    edit(thread: Thread, id: string, message: OutMessage): Promise<void>;
    /** Deletes a sent message (where the messenger can). */
    delete(thread: Thread, id: string): Promise<void>;
    /** Sets August's emoji reaction on any message, the user's too (empty removes it). */
    react(thread: Thread, id: string, emoji: string): Promise<void>;
    /** Opens a thread titled `title` inside `thread` (where `capabilities.open_thread`). */
    openThread(thread: Thread, title: string): Promise<{ messenger: string; id: string }>;
    /** Runs one of the messenger's `actions` in `thread`; `args` are checked against its
     *  `input_schema`. Resolves to its result. */
    action(thread: Thread, action: string, args?: object): Promise<any>;
    /** Saves an attachment of a message in `thread` (one of `message_in`'s `files`) to
     *  `path` (relative: in the workspace). */
    download(thread: Thread, file: any, path: string): Promise<{ path: string; size: number }>;
    /** Starts listening in `thread` for a press of one of `buttons` and/or (`text: true`) a text
     *  message; what it takes doesn't reach the agent. Listen before you send the question.
     *  The listener ends after `ttl` ms (default 10 minutes) even if `next` is never called. */
    listen(thread: Thread, opts: { buttons?: string[]; text?: boolean; ttl?: number }): Promise<number>;
    /** Waits for what the listener takes (default timeout 5 minutes); ends the listener. */
    next(listener: number, opts?: { timeout?: number }): Promise<Reply>;
    /** Asks in `thread` with `options` as buttons and waits (default 5 minutes): resolves to
     *  the option pressed, numbered or named, the user's own words, or null after the
     *  timeout; throws if the user cancelled it with /stop. Built on listen + send + next. */
    ask(thread: Thread, question: string, options: string[], opts?: { timeout?: number }): Promise<string | null>;
    /** Hands `thread` a message (see `PromptOpts`); passes `message_in`. */
    prompt(thread: Thread, text: string, opts?: PromptOpts): Promise<void>;
    /** Turns: start one and get its id; wait for its outcome (once;
     *  `{ status: "running" }` if the timeout passes first); cancel it; list running ones. */
    turns: {
      start(thread: Thread, turn: TurnRequest): Promise<number>;
      wait(id: number, opts?: { timeout?: number }): Promise<TurnOutcome>;
      cancel(id: number): Promise<boolean>;
      list(thread?: Thread): Promise<(Turn & { thread: Thread })[]>;
    };
    /** This extension's settings, kept in `config/extensions/<name>.json` and set by the user
     *  with `/config extensions.<name>.settings.<field> <value>` or `august config ...`. */
    settings: {
      /** Declares them: a JSON Schema whose `properties` may have a `default`; mark secrets
       *  with `secret: true` (shown as ••••). Call it in setup. */
      schema(schema: object): void;
      /** The current settings, defaults filled in. */
      get<T = Record<string, any>>(): Promise<T>;
      /** Changes one (`path` inside the settings, e.g. "timeout"); null deletes it. */
      set(path: string, value: unknown): Promise<void>;
    };
    /** Any unit's settings: `august.model`, `providers.openai.base_url`,
     *  `extensions.telegram.settings.allowed`, `extensions.web.settings`. Secrets come back masked;
     *  turning extensions on and off is the user's. Needs `config`. */
    config: {
      get(path: string): Promise<any>;
      set(path: string, value: unknown): Promise<void>;
    };
    /** Any operation of the core's table by name, e.g. `august.call("model_set", { model })`.
     *  The helpers below are the same calls, typed. */
    call(op: string, params?: object): Promise<any>;
    /** Every operation with the permission it needs. */
    ops(): Promise<Op[]>;
    /** Every agent tool and who offers it . */
    tools(): Promise<{ name: string; description: string; parameters: object; owner: string }[]>;
    /** Every slash command and who offers it. */
    commands(): Promise<{ name: string; description: string; owner: string }[]>;
    /** The model in use, the workspace, and whether `thread` is busy (null without one). */
    status(thread?: Thread): Promise<{ provider: string; model: string; workspace: string; busy: boolean | null }>;
    /** Stops everything running in `thread`, as /stop does; how many turns were running. Needs `turns`. */
    stop(thread: Thread): Promise<{ cancelled: number }>;
    /** Full-text search over everything said in any conversation. Needs `sessions`. */
    search(query: string, limit?: number): Promise<{ at: number; role: string; text: string }[]>;
    /** Switches to another model of the current provider (persisted). Needs `models`. */
    model: { set(model: string): Promise<{ provider: string; model: string }> };
    /** Your own entries in the journal (kind `custom`, `caller` you): state that follows a
     *  conversation, rebuilt with `sessions.history`. */
    journal: { append(where: { session?: string; thread?: Thread }, type: string, data?: unknown): Promise<number> };
    /** A thread's conversation. Need `sessions`. */
    sessions: {
      /** Stored conversations, newest first: of the chat that started them, or all. `bound`:
       *  the chats whose current conversation it is. */
      list(thread?: Thread): Promise<SessionInfo[]>;
      /** Starts a new conversation in `thread`, as /new does (`null`: one of no chat, addressed
       *  as `{ messenger: "session", id }`); returns its id. */
      new(thread: Thread | null, opts?: { name?: string; settings?: SessionSettings }): Promise<string>;
      /** Renames a conversation or changes its settings (a null field deletes it); takes
       *  effect from its next turn. */
      update(session: string, change: { name?: string; settings?: Partial<Record<keyof SessionSettings, any>> }): Promise<void>;
      /** Continues a stored conversation in `thread`. */
      switch(thread: Thread, session: string): Promise<string>;
      /** The journal of a conversation (`thread`: its current one; neither: everything):
       *  entries after `since` (an entry id), of `kinds`, at most `limit` (default 100, the
       *  newest), oldest first. */
      history(where: { session?: string; thread?: Thread }, opts?: { kinds?: JournalKind[]; since?: number; limit?: number }): Promise<JournalEntry[]>;
      /** The live conversation of `thread` (what the model sees next), with the input tokens
       *  last reported and the model's context window. */
      messages(thread: Thread): Promise<{ session: string; messages: Message[]; tokens: number; window: number | null }>;
      /** Replaces the live conversation of `thread` (earlier messages stay searchable). */
      setMessages(thread: Thread, messages: Message[]): Promise<void>;
    };
    /** Other extensions. Need `admin`. */
    extensions: {
      list(): Promise<ExtensionInfo[]>;
      /** Starts it and keeps it enabled; resolves to its status line. */
      enable(name: string): Promise<string>;
      /** Stops it and keeps it disabled. */
      disable(name: string): Promise<void>;
      /** Restarts every extension but this one. */
      reload(): Promise<ExtensionInfo[]>;
    };
  }

  /** An account signed in to with an API key: August asks for it (the message is deleted),
   *  runs `check` (throw to reject it) and keeps it as secret `id`. */
  export interface KeyAccount {
    id: string;
    label?: string;
    /** Model providers it unlocks. */
    providers?: string[];
    key: { label?: string; /** A variable that stands in for the key. */ env?: string };
    check?(key: string): Promise<void>;
  }

  /** An account with a sign-in of its own (OAuth, a device code, ...), scripted step by step. */
  export interface LoginAccount {
    id: string;
    label?: string;
    providers?: string[];
    login(steps: LoginSteps): Promise<{ who?: string; expiresAt?: number }>;
    /** Forget what it keeps (revoke tokens, delete secrets). */
    logout?(): Promise<void>;
  }

  /** The steps of a sign-in. August shows each where the user started it and brings the answer
   *  back; nothing else may talk to the user or open ports for it. */
  export interface LoginSteps {
    ask(label: string, opts?: { secret?: boolean }): Promise<string>;
    choose(question: string, options: string[]): Promise<string>;
    /** Shows a link to open (opened by itself at this machine's terminal). */
    open(url: string, note?: string): Promise<void>;
    progress(text: string): Promise<void>;
    /** Receives one redirect on http://localhost:<port><path> (port 0: any free one); returns
     *  that address, e.g. for an OAuth `redirect_uri`. Call before `open`. */
    callback(opts?: { port?: number; path?: string }): Promise<string>;
    /** The redirect's query parameters; from a browser elsewhere the user pastes its address. */
    waitCallback(opts?: { timeout?: number }): Promise<Record<string, string>>;
  }
}

export type Health = { status: "ok" | "degraded" | "failed"; detail?: string };
