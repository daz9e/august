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
    /** Queues a new agent turn in that chat with `text` as the user message. */
    prompt(text: string): Promise<void>;
  }

  /** What each event handler receives. */
  export interface Events {
    /** A user message arrived (before the agent sees it). */
    message_in: { text: string };
    /** A turn is about to start; `system` is the base system prompt. */
    before_turn: { text: string; system: string };
    /** The model wants to run a tool. */
    tool_call: { tool: string; input: any };
    /** A tool finished. */
    tool_result: { tool: string; input: any; output: string; isError: boolean };
    /** A turn finished with `reply` (observe only; runs in the background). */
    turn_end: { text: string; reply: string };
  }

  /** What a handler may return; returned fields replace the event's data. */
  export interface Results {
    /** `handled: true` swallows the message (optionally answering with `reply`). */
    message_in: { text?: string; handled?: boolean; reply?: string };
    before_turn: { text?: string; system?: string };
    /** `block` (a reason) stops the call; the model sees the reason as an error. */
    tool_call: { input?: any; block?: string };
    tool_result: { output?: string; isError?: boolean };
    turn_end: void;
  }

  export interface Tool<P = any> {
    /** Letters, digits, `_` and `-`; must not clash with a built-in tool. */
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
    on<E extends keyof Events>(
      event: E,
      handler: (data: Events[E], ctx: Context) => Results[E] | void | Promise<Results[E] | void>,
    ): void;
    registerTool<P = any>(tool: Tool<P>): void;
    /** `/name` in Telegram and the terminal. */
    registerCommand(name: string, command: Command | Command["handler"]): void;
    send(channel: string, chat: string, text: string): Promise<void>;
    prompt(channel: string, chat: string, text: string): Promise<void>;
  }
}
