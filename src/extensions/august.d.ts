// Types of the August extension API. Use with `import type { August } from "august";`
// (type-only imports are erased when bun runs the file).

declare module "august" {
  /** A conversation: `{ channel: "telegram", chat: "123" }`, or `cli` in the terminal. */
  export type Chat = { channel: string; chat: string };

  export interface Context {
    /** The chat the event, tool call or command belongs to. */
    chat: Chat | null;
    /** Sends a Markdown message to that chat. */
    send(text: string): Promise<void>;
    /** Hands the chat `text` as if the user sent it: joins the running turn, or starts one. */
    prompt(text: string): Promise<void>;
    /** Runs a sub-agent in this chat: a fresh conversation (it sees nothing of the chat),
     *  the chat's approvals, nobody to answer questions. Resolves to its final reply.
     *  `system` is added to the base system prompt; `tools` limits it to those tools,
     *  `exclude` hides some. Not available in the terminal. */
    agent(task: string, opts?: { system?: string; tools?: string[]; exclude?: string[] }): Promise<string>;
    /** Asks the user to pick one of `options` (buttons in a chat, a numbered list in the
     *  terminal); resolves to the chosen option, or null if they didn't answer in 5 minutes. */
    ask(question: string, options: string[]): Promise<string | null>;
    /** Asks the user (button or prompt) whether `action` may run; resolves to their answer. */
    approve(action: string): Promise<boolean>;
    /** Runs any agent tool (built-in, MCP or extension) in that chat, with its hooks and approvals. */
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
    message_in: { text: string; files: { path: string; mime: string; voice: boolean }[] };
    /** A turn is about to start; `system` is the base system prompt. */
    before_turn: { text: string; system: string };
    /** The model wants to run a tool. */
    tool_call: { tool: string; input: any };
    /** A tool finished. */
    tool_result: { tool: string; input: any; output: string; isError: boolean };
    /** A turn finished with `reply` (observe only; runs in the background). `unattended`:
     *  a scheduled task or a sub-agent, not the user's conversation. */
    turn_end: { text: string; reply: string; unattended: boolean };
    /** The user sent /stop in the chat (observe only). */
    stop: {};
    /** Before each model call of a turn; `step` counts from 0, `system` is the full prompt. */
    llm_call: { step: number; system: string };
    /** Before each model call, after `llm_call`: the conversation the model is about to see. */
    context: { step: number; messages: Message[] };
    /** After each model call (observe only; background). */
    llm_result: { step: number; text: string; toolCalls: { name: string; input: any }[]; usage: Usage };
    /** The chat started a new conversation, e.g. with /new (observe only; background). */
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
    stop: void;
    /** `system` replaces the system prompt for this one call (breaks the prompt cache). */
    llm_call: { system?: string };
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
    on<E extends keyof Events>(
      event: E,
      handler: (data: Events[E], ctx: Context) => Results[E] | void | Promise<Results[E] | void>,
    ): void;
    /** May be called any time; tools added or removed after setup show up from the next model call. */
    registerTool<P = any>(tool: Tool<P>): void;
    unregisterTool(name: string): void;
    /** `/name` in Telegram and the terminal. */
    registerCommand(name: string, command: Command | Command["handler"]): void;
    send(channel: string, chat: string, text: string): Promise<void>;
    prompt(channel: string, chat: string, text: string): Promise<void>;
  }
}
