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
type Message = string | { text: string; buttons?: Button[] | Button[][]; files?: string[]; reply_to?: string };

let nextId = 1;
const waiting = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

/** A request to the August core. */
function call(method: string, params: unknown): Promise<any> {
  const id = nextId++;
  write({ id, method, params });
  return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
}

const handlers = new Map<string, Function[]>();
const timeouts = new Map<string, number>();
/** In-flight requests from August, by id, so a cancel can abort them. */
const running = new Map<number, AbortController>();
const tools = new Map<string, any>();
const commands = new Map<string, any>();
const sections = new Map<string, string>();
const needs = new Set<string>();
const emits = new Map<string, { name: string; description: string; schema: unknown; observe: boolean }>();
const replaces = new Set<string>();
const takes = new Set<string>();
const accounts = new Map<string, any>();
let settingsSchema: unknown = null;
let healthCheck: (() => unknown) | null = null;
let about = { summary: "", details: "" };
let started = false;
let manifestQueued = false;

function manifest() {
  return {
    ...about,
    tools: [...tools.values()].map((t) => ({
      name: t.name,
      description: t.description,
      parameters: t.parameters ?? { type: "object", properties: {} },
    })),
    commands: [...commands.entries()].map(([n, c]) => ({ name: n, description: c.description ?? "" })),
    events: [...handlers.keys()],
    sections: [...sections.entries()].map(([name, text]) => ({ name, text })),
    timeouts: Object.fromEntries(timeouts),
    protocol: 2,
    needs: [...needs],
    settings: settingsSchema,
    emits: [...emits.values()],
    replaces: [...replaces],
    takes: [...takes],
    accounts: [...accounts.values()].map((a) => ({
      id: a.id,
      label: a.label ?? a.id,
      providers: a.providers ?? [],
      key: a.key ? { label: a.key.label ?? "API key", env: a.key.env ?? null } : null,
      login: typeof a.login === "function",
    })),
  };
}

/** The steps of a sign-in; August shows each where the user started it. */
function loginSteps(session: number) {
  const step = (op: string, params: object = {}) => call(op, { session, ...params });
  return {
    ask: (label: string, opts: { secret?: boolean } = {}): Promise<string> => step("login_ask", { label, secret: opts.secret ?? false }),
    choose: (question: string, options: string[]): Promise<string> => step("login_choose", { question, options }),
    open: (url: string, note = "") => step("login_open", { url, note }),
    progress: (text: string) => step("login_progress", { text }),
    /** Receives one redirect on http://localhost:<port><path> (port 0: any free one); returns that address. */
    callback: (opts: { port?: number; path?: string } = {}): Promise<string> => step("login_callback", { port: opts.port ?? 0, path: opts.path ?? "/callback" }),
    /** The redirect's query parameters (or those of the address the user pastes). */
    waitCallback: (opts: { timeout?: number } = {}): Promise<Record<string, string>> => step("login_wait", { timeout_ms: opts.timeout ?? 300_000 }),
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
  const listener = await call("listen", { thread, buttons: buttons.map((b) => b.id), text: true, ttl_ms: timeout + 5_000 });
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
  const none = reply?.cancelled ? "⏹ cancelled" : "⌛ no answer";
  await call("edit", { thread, id, message: `${text}\n→ ${answer ?? none}` }).catch(() => {});
  if (reply?.cancelled) throw new Error("the user cancelled the question (/stop)");
  return answer;
}

type Turn = { id: number; conversation: string; show: boolean; source?: string; parent?: number; meta?: unknown };

/** Starts a turn and waits for its outcome; throws unless it ended ok. */
async function runTurn(thread: Thread, turn: object): Promise<string> {
  const id = await call("turn_start", { thread, turn });
  const out = await call("turn_wait", { id });
  if (out.status === "ok") return out.reply;
  throw new Error(out.status === "cancelled" ? "cancelled (/stop)" : out.error ?? out.status);
}

function context(thread: Thread | null, turn: Turn | null = null, depth = 0) {
  const need = (): Thread => {
    if (!thread) throw new Error("this call has no thread");
    return thread;
  };
  const inThread = (method: string, params: object) =>
    thread ? call(method, { thread, from_turn: turn?.id, ...params }) : Promise.reject(new Error("this call has no thread"));
  return {
    thread,
    turn,
    send: (message: Message) => inThread("send", { message }),
    prompt: (text: string, opts: { source?: string; deliver?: string } = {}) => inThread("prompt", { text, ...opts }),
    agent: (task: string, opts: object = {}) => runTurn(need(), { ...opts, text: task, conversation: "new", parent: turn?.id }),
    ask: async (question: string, options: string[], opts: { timeout?: number } = {}) => ask(need(), question, options, opts.timeout),
    callTool: (name: string, input: unknown = {}) => inThread("callTool", { name, input }),
    llm: (prompt: string | object[], opts: { system?: string; effort?: string; options?: object } = {}) =>
      call("llm", { ...(typeof prompt === "string" ? { prompt } : { messages: prompt }), ...opts, thread, from_turn: turn?.id }),
    emit: (event: string, data: object = {}) => call("emit", { event, data, thread, from_turn: turn?.id, depth }),
  };
}

const api = {
  name,
  dir: dirname(entry),
  workspace: process.env.AUGUST_WORKSPACE ?? "",
  on(event: string, handler: Function, opts: { timeout?: number } = {}) {
    if (typeof handler !== "function") throw new Error(`on("${event}"): handler must be a function`);
    if (opts.timeout) timeouts.set(event, opts.timeout);
    handlers.set(event, [...(handlers.get(event) ?? []), handler]);
    changed();
  },
  defineEvent(event: string, spec: { description?: string; schema?: unknown; observe?: boolean } = {}) {
    emits.set(event, { name: event, description: spec.description ?? "", schema: spec.schema ?? null, observe: spec.observe ?? false });
    changed();
  },
  replaces(...extensions: string[]) {
    extensions.forEach((e) => replaces.add(e));
    changed();
  },
  takes(...jobs: string[]) {
    jobs.forEach((j) => takes.add(j));
    changed();
  },
  emit: (event: string, data: object = {}) => call("emit", { event, data }),
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
  needs(...permissions: string[]) {
    permissions.forEach((p) => needs.add(p));
    // During setup, at once: calls the setup makes next are checked against it.
    if (!started) write({ method: "manifest", params: manifest() });
    changed();
  },
  /** How it is doing, asked every so often: `{status: "ok" | "degraded" | "failed", detail}`.
   * `degraded`: something outside is wrong (restarting won't help); `failed`: broken inside,
   * August restarts it. */
  health(check: () => unknown) {
    if (typeof check !== "function") throw new Error("health(check): check must be a function");
    healthCheck = check;
  },
  describe(summary: string, details: string) {
    if (typeof summary !== "string" || typeof details !== "string") throw new Error("describe(summary, details): both must be strings");
    about = { summary: summary.trim(), details: details.trim() };
    changed();
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
  /** Something the user signs in to, through August (`/login`): `{id, label, providers,
   * key: {label, env}, check(key)}` for an API key August asks for, or `{id, label, providers,
   * login(steps) => ({who}), logout()}` for a sign-in of its own (OAuth, codes, ...). */
  registerAccount(account: any) {
    if (!account?.id) throw new Error("registerAccount: id is required");
    if (!account.key && typeof account.login !== "function") throw new Error(`registerAccount("${account.id}"): key or login is required`);
    accounts.set(account.id, account);
    changed();
  },
  unregisterAccount(id: string) {
    if (accounts.delete(id)) changed();
  },
  /** How an account stands: connected (by who), expired (the user is told) or none. */
  accountUpdate: (id: string, status: string, who?: string) => call("account_update", { account: id, status, who }),
  /** This extension's secrets (an account's API key is under the account's id). */
  secrets: {
    get: (key: string): Promise<string | null> => call("secret_get", { key }),
    set: (key: string, value: string | null) => call("secret_set", { key, value }),
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
  delete: (thread: Thread, id: string) => call("delete", { thread, id }),
  react: (thread: Thread, id: string, emoji: string) => call("react", { thread, id, emoji }),
  openThread: (thread: Thread, title: string) => call("open_thread", { thread, title }),
  action: (thread: Thread, action: string, args: object = {}) => call("action", { thread, action, args }),
  download: (thread: Thread, file: any, path: string) => call("download", { thread, file, path }),
  listen: (thread: Thread, opts: { buttons?: string[]; text?: boolean; ttl?: number } = {}) =>
    call("listen", { thread, buttons: opts.buttons ?? [], text: opts.text ?? false, ttl_ms: opts.ttl ?? 600_000 }),
  next: (listener: number, opts: { timeout?: number } = {}) => call("next", { listener, timeout_ms: opts.timeout ?? 300_000 }),
  ask: (thread: Thread, question: string, options: string[], opts: { timeout?: number } = {}) => ask(thread, question, options, opts.timeout),
  prompt: (thread: Thread, text: string, opts: { source?: string; deliver?: string } = {}) => call("prompt", { thread, text, ...opts }),
  turns: {
    start: (thread: Thread, turn: object) => call("turn_start", { thread, turn }),
    wait: (id: number, opts: { timeout?: number } = {}) => call("turn_wait", { id, timeout_ms: opts.timeout }),
    cancel: (id: number) => call("turn_cancel", { id }),
    list: (thread?: Thread) => call("turns", { thread }),
  },
  /** This extension's settings: declare a schema, read them (defaults filled in), change one. */
  settings: {
    schema(schema: unknown) {
      settingsSchema = schema;
      changed();
    },
    get: () => call("settings", {}),
    set: (path: string, value: unknown) => call("settings_set", { path, value: value ?? null }),
  },
  /** Any unit's settings by path (`august.model`, `extensions.web.settings`); needs `config`. */
  config: {
    get: (path: string) => call("config_get", { path }),
    set: (path: string, value: unknown) => call("config_set", { path, value: value ?? null }),
  },
  /** Any operation of the core's table by name (`august.ops()` lists them). */
  call: (op: string, params: object = {}) => call(op, params),
  ops: () => call("ops", {}),
  journal: {
    append: (where: { session?: string; thread?: Thread }, type: string, data?: unknown) => call("journal_append", { ...where, type, data }),
  },
  tools: () => call("tools", {}),
  commands: () => call("commands", {}),
  status: (thread?: Thread) => call("status", { thread }),
  stop: (thread: Thread) => call("stop", { thread }),
  search: (query: string, limit?: number) => call("search", { query, limit }),
  model: { set: (model: string) => call("model_set", { model }) },
  sessions: {
    list: (thread?: Thread) => call("sessions", { thread }),
    new: (thread: Thread | null, opts: { name?: string; settings?: object } = {}) => call("session_new", { ...(thread ? { thread } : {}), ...opts }),
    update: (session: string, change: { name?: string; settings?: object }) => call("session_update", { session, ...change }),
    switch: (thread: Thread, session: string) => call("session_switch", { thread, session }),
    history: (where: { session?: string; thread?: Thread }, opts: { kinds?: string[]; since?: number; limit?: number } = {}) =>
      call("history", { ...where, ...opts }),
    messages: (thread: Thread) => call("messages", { thread }),
    setMessages: (thread: Thread, messages: object[]) => call("messages_set", { thread, messages }),
  },
  extensions: {
    list: () => call("extensions", {}),
    enable: (name: string) => call("extension_enable", { name }),
    disable: (name: string) => call("extension_disable", { name }),
    reload: () => call("extensions_reload", {}),
  },
};

async function handle(method: string, params: any, signal: AbortSignal): Promise<unknown> {
  const ctx = { ...context(params.ctx?.thread ?? null, params.ctx?.turn ?? null, params.ctx?.depth ?? 0), signal };
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
    case "health":
      return healthCheck ? await healthCheck() : { status: "ok" };
    case "account_check":
    case "login":
    case "logout": {
      const account = accounts.get(params.account);
      if (!account) throw new Error(`no account named ${params.account}`);
      if (method === "account_check") return account.check ? ((await account.check(params.key)), null) : null;
      if (method === "logout") return account.logout ? ((await account.logout()), null) : null;
      const signed = await account.login(loginSteps(params.session));
      return { who: signed?.who ?? null, expires_at: signed?.expiresAt ?? null };
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
    if (msg.method === "cancel") {
      running.get(msg.params?.id)?.abort();
    } else if (msg.method) {
      const controller = new AbortController();
      running.set(msg.id, controller);
      handle(msg.method, msg.params ?? {}, controller.signal)
        .finally(() => running.delete(msg.id))
        .then(
          (result) => write({ id: msg.id, result: result ?? null }),
          (e) => {
            if (controller.signal.aborted) return; // nobody waits for it any more
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
