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
  export type TurnMode = "visible" | "quiet" | "fork" | "fresh";
  export type Turn = { id: number; mode: TurnMode; source?: string; parent?: number };
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
    /** fork: run approvals without asking (nobody may be there to ask). */
    approve_all?: boolean;
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
    /** Hands the thread `text` as if the user sent it: joins the running turn, or starts one. */
    prompt(text: string): Promise<void>;
    /** Runs a sub-agent for this thread (a `fresh` turn under this one): a new conversation,
     *  the thread's approvals, nobody to answer questions. Resolves to its final reply; throws
     *  if it failed or was cancelled (/stop cancels all of a thread's turns).
     *  `system` is added to the base system prompt; `tools` limits it to those tools,
     *  `exclude` hides some. */
    agent(task: string, opts?: { system?: string; tools?: string[]; exclude?: string[] }): Promise<string>;
    /** `august.ask` in this thread. */
    ask(question: string, options: string[], opts?: { timeout?: number }): Promise<string | null>;
    /** Asks the user whether `action` may run (August's approval: Allow / Deny, 5 minutes). */
    approve(action: string): Promise<boolean>;
    /** Runs any agent tool (built-in, MCP or extension) for this thread, with its hooks and approvals. */
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
      /** The message's id in its messenger (for `react`, `reply_to`). */
      id: string;
      text: string;
      files: { path: string; mime: string; kind: "voice" | "audio" | "image" | "video" | "document"; voice: boolean }[];
    };
    /** A turn is about to start; `system` is the base system prompt. */
    before_turn: { text: string; system: string };
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
    /** Before each model call of a turn; `step` counts from 0, `system` is the full prompt. */
    llm_call: { step: number; system: string; model: string; tools: string[] };
    /** Before each model call, after `llm_call`: the conversation the model is about to see. */
    context: { step: number; messages: Message[] };
    /** After each model call (observe only; background). */
    llm_result: { step: number; text: string; toolCalls: { name: string; input: any }[]; usage: Usage };
    /** The thread started a new conversation, e.g. with /new (observe only; background). */
    session_start: { previous: string; session: string };
    /** Older history was summarised; estimated tokens (observe only; background). */
    compaction: { before: number; after: number };
  }

  /** What a handler may return; returned fields replace the event's data. */
  export interface Results {
    /** `handled: true` swallows the message (optionally answering with `reply`). */
    message_in: { text?: string; handled?: boolean; reply?: string };
    before_turn: { text?: string; system?: string };
    /** `block` (a reason) stops the call; the model sees the reason as an error. */
    tool_call: { input?: any; block?: string; approve?: boolean; ask?: string };
    tool_result: { output?: string; isError?: boolean };
    turn_end: void;
    extension_state: void;
    config_changed: void;
    stop: void;
    reaction: void;
    shutdown: void;
    /** `system` replaces the system prompt for this one call (breaks the prompt cache). */
    llm_call: { system?: string; model?: string; tools?: string[] };
    /** Returned `messages` replace what the model sees for this one call; the stored
     *  history is unchanged (keep tool_use / tool_result pairs intact). */
    context: { messages?: Message[] };
    llm_result: void;
    session_start: void;
    compaction: void;
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
     *  answering in the thread of the call in progress, the store and approvals need nothing. */
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
    /** Hands `thread` a message as if the user sent it. */
    prompt(thread: Thread, text: string): Promise<void>;
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
    /** A thread's conversation. Need `sessions`. */
    sessions: {
      /** Starts a new conversation, as /new does. */
      new(thread: Thread): Promise<void>;
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
