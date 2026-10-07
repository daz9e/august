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

type Thread = { messenger: string; id: string };
type Button = { id: string; label: string };
type Message = string | { text: string; buttons?: Button[] };

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
const sections = new Map<string, string>();
let started = false;
let manifestQueued = false;

function manifest() {
  return {
    tools: [...tools.values()].map((t) => ({
      name: t.name,
      description: t.description,
      parameters: t.parameters ?? { type: "object", properties: {} },
    })),
    commands: [...commands.entries()].map(([n, c]) => ({ name: n, description: c.description ?? "" })),
    events: [...handlers.keys()],
    sections: [...sections.entries()].map(([name, text]) => ({ name, text })),
  };
}

/** After startup, tells August what changed (once per tick, however many calls). */
function changed() {
  if (!started || manifestQueued) return;
  manifestQueued = true;
  queueMicrotask(() => {
    manifestQueued = false;
    write({ method: "manifest", params: manifest() });
  });
}

/** Asks in `thread`: the question with `options` as buttons; the answer is a press, a
 * number, an option's name or the user's own words. Null after `timeout` ms. */
async function ask(thread: Thread, question: string, options: string[], timeout = 300_000): Promise<string | null> {
  const key = Math.random().toString(36).slice(2, 10);
  const buttons = options.map((label, i) => ({ id: `${key}.${i}`, label }));
  // Listen before sending, so a quick answer can't slip past.
  const listener = await call("listen", { thread, buttons: buttons.map((b) => b.id), text: true });
  const text = `❓ ${question}`;
  const id = await call("send", { thread, message: { text, buttons } });
  const reply = await call("next", { listener, timeout_ms: timeout });
  let answer: string | null = null;
  if (reply?.press) answer = options[buttons.findIndex((b) => b.id === reply.press)] ?? null;
  else if (typeof reply?.text === "string") {
    const n = Number(reply.text.trim());
    const named = options.find((o) => o.toLowerCase() === reply.text.trim().toLowerCase());
    answer = (Number.isInteger(n) && options[n - 1]) || named || reply.text;
  }
  await call("edit", { thread, id, message: `${text}\n→ ${answer ?? "⌛ no answer"}` }).catch(() => {});
  return answer;
}

function context(thread: Thread | null) {
  const need = (): Thread => {
    if (!thread) throw new Error("this call has no thread");
    return thread;
  };
  const inThread = (method: string, params: object) => (thread ? call(method, { thread, ...params }) : Promise.reject(new Error("this call has no thread")));
  return {
    thread,
    send: (message: Message) => inThread("send", { message }),
    prompt: (text: string) => inThread("prompt", { text }),
    agent: (task: string, opts: object = {}) => inThread("agent", { task, opts }),
    ask: async (question: string, options: string[], opts: { timeout?: number } = {}) => ask(need(), question, options, opts.timeout),
    approve: (action: string) => inThread("approve", { action }),
    callTool: (name: string, input: unknown = {}) => inThread("callTool", { name, input }),
    llm: (prompt: string, opts: { system?: string } = {}) => call("llm", { prompt, system: opts.system }),
  };
}

const api = {
  name,
  dir: dirname(entry),
  workspace: process.env.AUGUST_WORKSPACE ?? "",
  on(event: string, handler: Function) {
    if (typeof handler !== "function") throw new Error(`on("${event}"): handler must be a function`);
    handlers.set(event, [...(handlers.get(event) ?? []), handler]);
    changed();
  },
  registerTool(tool: any) {
    if (!tool?.name || !tool?.description || typeof tool?.execute !== "function") {
      throw new Error("registerTool: name, description and execute are required");
    }
    tools.set(tool.name, tool);
    changed();
  },
  unregisterTool(name: string) {
    if (tools.delete(name)) changed();
  },
  registerPromptSection(name: string, text: string) {
    if (typeof text !== "string") throw new Error(`registerPromptSection("${name}"): text must be a string`);
    sections.set(name, text);
    changed();
  },
  registerCommand(cmd: string, spec: any) {
    const command = typeof spec === "function" ? { handler: spec } : spec;
    if (typeof command?.handler !== "function") throw new Error(`registerCommand("${cmd}"): handler is required`);
    commands.set(cmd.replace(/^\//, ""), command);
    changed();
  },
  /** This extension's storage: JSON values by key, kept across restarts. */
  store: {
    get: (key: string) => call("store_get", { key }),
    set: (key: string, value: unknown) => call("store_set", { key, value: value ?? null }),
    delete: (key: string) => call("store_set", { key, value: null }),
    list: (prefix = "") => call("store_list", { prefix }),
  },
  messengers: () => call("messengers", {}),
  send: (thread: Thread, message: Message) => call("send", { thread, message }),
  edit: (thread: Thread, id: string, message: Message) => call("edit", { thread, id, message }),
  listen: (thread: Thread, opts: { buttons?: string[]; text?: boolean } = {}) =>
    call("listen", { thread, buttons: opts.buttons ?? [], text: opts.text ?? false }),
  next: (listener: number, opts: { timeout?: number } = {}) => call("next", { listener, timeout_ms: opts.timeout ?? 300_000 }),
  ask: (thread: Thread, question: string, options: string[], opts: { timeout?: number } = {}) => ask(thread, question, options, opts.timeout),
  prompt: (thread: Thread, text: string) => call("prompt", { thread, text }),
};

async function handle(method: string, params: any): Promise<unknown> {
  const ctx = context(params.ctx?.thread ?? null);
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

started = true;
write({ method: "ready", params: manifest() });
