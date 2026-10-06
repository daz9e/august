// August extension host. Loads one extension and talks to the August core over
// stdin/stdout with newline-delimited JSON-RPC. stdout is reserved for the protocol;
// console output goes to stderr, which August writes to its log.
//
// Usage: bun run host.ts <entry file> <extension name>

import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";
import { dirname } from "node:path";

const [entry, name] = process.argv.slice(2);

for (const level of ["log", "info", "warn", "error", "debug"] as const) {
  console[level] = (...args: unknown[]) => {
    process.stderr.write(args.map((a) => (typeof a === "string" ? a : Bun.inspect(a))).join(" ") + "\n");
  };
}

const write = (msg: unknown) => process.stdout.write(JSON.stringify(msg) + "\n");

type Chat = { channel: string; chat: string };

let nextId = 1;
const waiting = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

/** A request to the August core. */
function call(method: string, params: unknown): Promise<any> {
  const id = nextId++;
  write({ id, method, params });
  return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
}

const handlers = new Map<string, Function[]>();
const tools = new Map<string, any>();
const commands = new Map<string, any>();

function context(chat: Chat | null) {
  const noChat = () => Promise.reject(new Error("this call has no chat"));
  return {
    chat,
    send: (text: string) => (chat ? call("send", { ...chat, text }) : noChat()),
    prompt: (text: string) => (chat ? call("prompt", { ...chat, text }) : noChat()),
    callTool: (name: string, input: unknown = {}) => (chat ? call("callTool", { ...chat, name, input }) : noChat()),
    llm: (prompt: string, opts: { system?: string } = {}) => call("llm", { prompt, system: opts.system }),
  };
}

const api = {
  name,
  dir: dirname(entry),
  on(event: string, handler: Function) {
    if (typeof handler !== "function") throw new Error(`on("${event}"): handler must be a function`);
    handlers.set(event, [...(handlers.get(event) ?? []), handler]);
  },
  registerTool(tool: any) {
    if (!tool?.name || !tool?.description || typeof tool?.execute !== "function") {
      throw new Error("registerTool: name, description and execute are required");
    }
    tools.set(tool.name, tool);
  },
  registerCommand(cmd: string, spec: any) {
    const command = typeof spec === "function" ? { handler: spec } : spec;
    if (typeof command?.handler !== "function") throw new Error(`registerCommand("${cmd}"): handler is required`);
    commands.set(cmd.replace(/^\//, ""), command);
  },
  send: (channel: string, chat: string, text: string) => call("send", { channel, chat, text }),
  prompt: (channel: string, chat: string, text: string) => call("prompt", { channel, chat, text }),
};

async function handle(method: string, params: any): Promise<unknown> {
  const ctx = context(params.ctx?.chat ?? null);
  switch (method) {
    case "event": {
      // Handlers run in order; each result is merged into the data the next one sees.
      const data = params.data ?? {};
      for (const handler of handlers.get(params.name) ?? []) {
        const result = await handler(data, ctx);
        if (result && typeof result === "object") Object.assign(data, result);
        if (data.block || data.handled) break;
      }
      return data;
    }
    case "tool": {
      const tool = tools.get(params.name);
      if (!tool) throw new Error(`no tool named ${params.name}`);
      const out = await tool.execute(params.input ?? {}, ctx);
      return typeof out === "string" ? out : JSON.stringify(out ?? null);
    }
    case "command": {
      const command = commands.get(params.name);
      if (!command) throw new Error(`no command named ${params.name}`);
      const reply = await command.handler(params.args ?? "", ctx);
      return typeof reply === "string" ? reply : null;
    }
  }
  throw new Error(`unknown method ${method}`);
}

createInterface({ input: process.stdin })
  .on("line", (line) => {
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      return;
    }
    if (msg.method) {
      handle(msg.method, msg.params ?? {}).then(
        (result) => write({ id: msg.id, result: result ?? null }),
        (e) => {
          console.error(`${msg.method} failed:`, e);
          write({ id: msg.id, error: { message: e instanceof Error ? e.message : String(e) } });
        },
      );
    } else {
      const w = waiting.get(msg.id);
      if (!w) return;
      waiting.delete(msg.id);
      if (msg.error) w.reject(new Error(msg.error.message));
      else w.resolve(msg.result);
    }
  })
  .on("close", () => process.exit(0));

try {
  const init = (await import(pathToFileURL(entry).href)).default;
  if (typeof init !== "function") throw new Error("index.ts must `export default function (august) { ... }`");
  await init(api);
} catch (e) {
  console.error(e);
  process.exit(1);
}
// From here on a stray error in a handler must not kill the extension.
process.on("unhandledRejection", (e) => console.error("unhandled rejection:", e));
process.on("uncaughtException", (e) => console.error("uncaught exception:", e));

write({
  method: "ready",
  params: {
    tools: [...tools.values()].map((t) => ({
      name: t.name,
      description: t.description,
      parameters: t.parameters ?? { type: "object", properties: {} },
    })),
    commands: [...commands.entries()].map(([n, c]) => ({ name: n, description: c.description ?? "" })),
    events: [...handlers.keys()],
  },
});
