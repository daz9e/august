// Types of the August extension API. Use with `import type { August } from "august";`
// (type-only imports are erased when bun runs the file).

declare module "august" {
  /** A conversation in a messenger: a Telegram chat (`{ messenger: "telegram", id: "123" }`),
   *  a terminal window (`{ messenger: "cli", id: "1" }`), ... Where a call takes a thread,
   *  `"home"` is the user's home thread (set with /home; else the one they wrote in last). */
  export type Thread = { messenger: string; id: string } | "home";

  /** A button under a message; a press comes back with its `id`. */
  export type Button = { id: string; label: string };
  /** What to send: Markdown text, optionally with buttons (one row, or rows), local files (the
   *  text is their caption) and the id of a message it answers. Each messenger renders all of
   *  it its own way, degrading what it can't show. */
  export type OutMessage = string | { text: string; buttons?: Button[] | Button[][]; files?: string[]; reply_to?: string };

  /** What a messenger says about itself. */
  export interface Messenger {
    id: string;
    name: string;
    capabilities: {
      markdown: boolean; max_len: number; buttons: number; edit: boolean; edit_interval_ms: number;
      files_in: boolean; files_out: boolean; images: boolean; audio_in: boolean;
      commands: boolean; presence: boolean; delete: boolean; reactions: boolean; reply: boolean; threads: boolean;
    };
    /** Anything else it offers, free form. */
    extra: Record<string, unknown>;
    /** Threads it knows of; `active` is the one the user wrote in last. */
    threads: { id: string; active: boolean; /** Unix ms of the last message from it. */ last_seen: number | null }[];
  }

  /** What a listener took: a button press or a text message; or why nothing came. */
  export type Reply = { press: string } | { text: string } | { timeout: true } | { cancelled: "stop" | "new" };

  /** A run of the agent. `visible`: the user's conversation, streamed to the thread; `quiet`:
   *  in the thread's conversation, nothing shown, the reply returned; `fork`: on a copy of the
   *  conversation (same prompt and tools, so the provider's cache holds), nothing kept;
   *  `fresh`: a new conversation (a sub-agent). */
  /** `source`: who it is from (default `ext:<your name>`; `user` passes it as the user's);
   *  the model sees `[from <source>]` before it. `deliver`: `steer` (default) joins the
   *  running turn before its next model call or starts one; `followUp` runs as its own turn
   *  after the current one; `nextTurn` waits for the next turn without starting one. */
  export type PromptOpts = { source?: string; deliver?: "steer" | "followUp" | "nextTurn" };
  export type TurnMode = "visible" | "quiet" | "fork" | "fresh";
  export type Turn = { id: number; mode: TurnMode; source?: string; parent?: number; meta?: unknown };
  export type TurnRequest = {
    text: string;
    mode: "quiet" | "fork" | "fresh";
    /** Who starts it; hooks see it as `ctx.turn.source`. */
    source?: string;
    /** The turn this one belongs to (e.g. `ctx.turn.id`). */
    parent?: number;
    /** fresh: instructions added to the base system prompt. */
    system?: string;
    /** fresh: only these tools. fork: only these may be called (all stay offered). */
    tools?: string[];
    /** fresh: never these tools. */
    exclude?: string[];
    /** Anything for hooks to read as `ctx.turn.meta` (e.g. `{ approve: "all" }` for the
     *  default approvals extension); the core doesn't look inside. */
    meta?: unknown;
  };
  export type TurnOutcome = {
    status: "ok" | "error" | "cancelled" | "running";
    reply: string;
    error?: string | null;
    /** fork: every tool call, `{ name, input, output, isError }`. */
    toolCalls: { name: string; input: any; output: string; isError: boolean }[];
  };

  /** What an extension declares it uses (`august.needs`); each operation needs at most one. */
  export type Permission = "messaging" | "turns" | "tools" | "llm" | "models" | "sessions" | "memory" | "config" | "admin";

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
   *  `turn_start` {mode, parent, text}, `turn_end` {status, error}, `user_message` {text},
   *  `assistant` {step, text, toolCalls, usage}, `tool` {tool, id, input, output, isError},
   *  `compaction` {before, after}, `session` {reason: new|switch, previous},
   *  `model_change` {model, previous}, `custom` {type, data} (an extension's own). */
  export type JournalKind = "turn_start" | "turn_end" | "user_message" | "assistant" | "tool" | "compaction" | "session" | "model_change" | "custom";
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

  export type UsageTotal = { calls: number; input: number; output: number; cache_read: number; cache_write: number };

  export type ExtensionInfo = {
    name: string;
    state: "running" | "failed" | "disabled";
    error: string | null;
    /** Who installed it: shipped with August, the user, or the agent (`save_extension`). */
    origin: "default" | "user" | "agent";
    /** Only while running: what it registered, and the built-in tools it replaces. */
    tools?: string[]; replaces?: string[]; commands?: string[]; hooks?: string[]; needs?: Permission[]; sections?: string[];
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
    /** Runs a sub-agent for this thread (a `fresh` turn under this one): a new conversation,
     *  nobody to answer questions. Resolves to its final reply; throws
     *  if it failed or was cancelled (/stop cancels all of a thread's turns).
     *  `system` is added to the base system prompt; `tools` limits it to those tools,
     *  `exclude` hides some. */
    agent(task: string, opts?: { system?: string; tools?: string[]; exclude?: string[] }): Promise<string>;
    /** `august.ask` in this thread. */
    ask(question: string, options: string[], opts?: { timeout?: number }): Promise<string | null>;
    /** Runs any agent tool (built-in, MCP or extension) for this thread, with its hooks. */
    callTool(name: string, input?: object): Promise<{ output: string; isError: boolean }>;
    /** One completion on the configured model, without tools; returns the text. */
    llm(prompt: string, opts?: { system?: string }): Promise<string>;
  }

  export type Block =
    | { type: "text"; text: string }
    | { type: "tool_use"; id: string; name: string; input: any }
    | { type: "tool_result"; tool_use_id: string; content: string; is_error: boolean }
    | { type: "image"; media_type: string; path: string }
    | { type: "opaque"; value: any };
  export type Message = { role: "user" | "assistant"; content: Block[] };

  export type Usage = { inputTokens: number; outputTokens: number; cacheReadTokens: number; cacheWriteTokens: number };

  /** What each event handler receives. */
  export interface Events {
    /** A user message arrived (before the agent sees it). `files`: its attachments, already
     *  saved in the workspace (`path` is absolute); `voice` marks a recorded voice note. */
    message_in: {
      /** The message's id in its messenger (for `react`, `reply_to`); null for a `prompt`. */
      id: string | null;
      text: string;
      files: { path: string; mime: string; kind: "voice" | "audio" | "image" | "video" | "document"; voice: boolean }[];
      /** `user` for what came from a messenger, else the `source` of a `prompt`. */
      source: string;
    };
    /** A turn is about to start; `system` is the base system prompt. */
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
      /** Not the user's visible conversation (`ctx.turn.mode` says which). */
      unattended: boolean;
    };
    /** What a visible turn does, in order, as it happens: `text` (reply fragments; ones that
     *  arrive while handlers run come merged), `step` (a new model call after tool results),
     *  `tool` (a call starts), `compacted`. Observe only; background. */
    turn_event:
      | { kind: "text"; text: string }
      | { kind: "step" }
      | { kind: "tool"; tool: string; input: any }
      | { kind: "compacted" };
    /** Nothing runs in `ctx.thread` any more and nothing is about to: every turn ended, no
     *  message waits, and the `turn_end` handlers (which may start the next turn) are done.
     *  Once per quiet period. Observe only; background. */
    turn_settled: {};
    /** The user reacted to `message` with `emoji` (empty: took it back). Observe only. */
    reaction: { message: string; emoji: string };
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
    /** Before each model call, after `llm_call`: the conversation the model is about to see. */
    context: { step: number; messages: Message[] };
    /** After each model call (observe only; background). */
    llm_result: { step: number; text: string; toolCalls: { name: string; input: any }[]; usage: Usage };
    /** Before a conversation's first turn (`start`: the thread's first; `new`: after /new). */
    session_start: { session: string; previous: string | null; reason: "start" | "new"; chat: string };
    /** The thread's conversation was replaced (/new, a switch), right away. Observe only. */
    session_changed: { reason: "new" | "switch"; previous: string; session: string };
    /** /new or `sessions.new` is about to start a conversation in `ctx.thread`. */
    session_before_new: { session: string | null; to: null; by: string };
    /** `sessions.switch` is about to continue conversation `to` in `ctx.thread`. */
    session_before_switch: { session: string | null; to: string; by: string };
    /** Older history was summarised; estimated tokens (observe only; background). */
    compaction: { before: number; after: number; reason: "threshold" | "manual" | "overflow"; fromExtension: boolean | null };
    /** Older history (`messages`) is about to be summarised; the last `kept` stay as they are. */
    session_before_compact: {
      reason: "threshold" | "manual" | "overflow";
      tokens: number;
      messages: Message[];
      previousSummary: string | null;
      kept: number;
    };
  }

  /** What a handler may return; returned fields replace the event's data. */
  export interface Results {
    /** `handled: true` swallows the message (optionally answering with `reply`). */
    message_in: { text?: string; handled?: boolean; reply?: string };
    before_turn: { text?: string; system?: string };
    /** Changed fields replace the message's; `block: true` drops it. */
    message_out: { text?: string; buttons?: { id: string; label: string }[][]; files?: string[]; block?: boolean };
    /** `block` (a reason) stops the call; the model sees the reason as an error. */
    tool_call: { input?: any; block?: string };
    tool_result: { output?: string; isError?: boolean };
    turn_end: void;
    turn_settled: void;
    turn_event: void;
    extension_state: void;
    config_changed: void;
    stop: void;
    reaction: void;
    shutdown: void;
    /** `model` switches to another one instead; `block` (a reason) refuses the switch. */
    model_select: { model?: string; block?: string };
    /** `system` replaces the system prompt for this one call (breaks the prompt cache). */
    llm_call: { system?: string; model?: string; tools?: string[] };
    /** Returned `messages` replace what the model sees for this one call; the stored
     *  history is unchanged (keep tool_use / tool_result pairs intact). */
    context: { messages?: Message[] };
    llm_result: void;
    /** The conversation's own settings, kept with it. */
    session_start: SessionSettings;
    session_changed: void;
    session_before_new: { block?: string };
    session_before_switch: { block?: string };
    compaction: void;
    /** `cancel` skips the summary; `summary` is used instead of asking the model. */
    session_before_compact: { cancel?: boolean; summary?: string };
  }

  export interface Tool<P = any> {
    /** Letters, digits, `_` and `-`; a built-in tool's name replaces that tool. */
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
    /** May be called any time; tools added or removed after setup show up from the next model call. */
    registerTool<P = any>(tool: Tool<P>): void;
    unregisterTool(name: string): void;
    /** Declares what this extension uses beyond its own thread (shown in /extensions):
     *  `messaging` (messengers, sending to or listening in any thread, prompt), `turns`
     *  (starting turns, sub-agents), `tools` (callTool), `llm`. Without it, those calls fail;
     *  answering in the thread of the call in progress, and the store need nothing. */
    needs(...permissions: Permission[]): void;
    /** A section of the system prompt (Markdown, e.g. "## Reminders\n..."): how and when the
     *  model should use what this extension offers. Fixed for each conversation, so a change
     *  shows up from the next one (`/new`). Registering a name again replaces it. */
    registerPromptSection(name: string, text: string): void;
    /** `/name` in Telegram and the terminal. */
    registerCommand(name: string, command: Command | Command["handler"]): void;
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
    /** Turns: start a quiet, fork or fresh one and get its id; wait for its outcome (once;
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
     *  `messengers.telegram.allowed`, `extensions.web.settings`. Secrets come back masked;
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
    /** Every agent tool and who offers it (`august` for built-in ones). */
    tools(): Promise<{ name: string; description: string; parameters: object; owner: string }[]>;
    /** Every slash command and who offers it. */
    commands(): Promise<{ name: string; description: string; owner: string }[]>;
    /** The model in use, the workspace, and whether `thread` is busy (null without one). */
    status(thread?: Thread): Promise<{ provider: string; model: string; workspace: string; busy: boolean | null }>;
    /** Stops everything running in `thread`, as /stop does; how many turns were running. Needs `turns`. */
    stop(thread: Thread): Promise<{ cancelled: number }>;
    /** The facts August remembers. Needs `memory`. */
    memory(): Promise<{ id: number; text: string }[]>;
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
      /** Starts a new conversation, as /new does; returns its id. */
      new(thread: Thread, opts?: { name?: string; settings?: SessionSettings }): Promise<string>;
      /** Renames a conversation or changes its settings (a null field deletes it); takes
       *  effect from its next turn. */
      update(session: string, change: { name?: string; settings?: Partial<Record<keyof SessionSettings, any>> }): Promise<void>;
      /** Continues a stored conversation in `thread`. */
      switch(thread: Thread, session: string): Promise<string>;
      /** The journal of a conversation (`thread`: its current one; neither: everything):
       *  entries after `since` (an entry id), of `kinds`, at most `limit` (default 100, the
       *  newest), oldest first. */
      history(where: { session?: string; thread?: Thread }, opts?: { kinds?: JournalKind[]; since?: number; limit?: number }): Promise<JournalEntry[]>;
      /** Summarises older messages; estimated tokens, or null if there was nothing to do. */
      compact(thread: Thread): Promise<{ before: number; after: number } | null>;
      /** Token usage of its current conversation, and of today across all threads. */
      usage(thread: Thread): Promise<{ session: UsageTotal; today: UsageTotal }>;
    };
    /** Other extensions. Need `admin`. */
    extensions: {
      list(): Promise<ExtensionInfo[]>;
      /** Starts it and keeps it enabled; resolves to its status line. */
      enable(name: string): Promise<string>;
      /** Stops it and keeps it disabled. */
      disable(name: string): Promise<void>;
      /** Restarts every extension, this one too. */
      reload(): Promise<ExtensionInfo[]>;
    };
  }
}
