// MCP client: the servers in `AUGUST_HOME/mcp.json` lend their tools to the agent as
// `mcp_<server>_<tool>`. Stdio servers run as child processes (one JSON-RPC message per
// line); `url` servers speak streamable HTTP. Servers that answer within SETUP_WAIT_MS
// are ready for the first message, slower ones add their tools when they connect. A
// server that fails to start or crashes is reported by `/mcp` and its tools removed.

import type { August } from "august";
import { join } from "node:path";
import { homedir } from "node:os";

type ServerConfig = {
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  headers?: Record<string, string>;
};

const PROTOCOL = "2025-06-18";
const START_TIMEOUT_MS = 30_000;
const CALL_TIMEOUT_MS = 600_000;
/// How long setup waits for servers before the agent starts without the slow ones.
const SETUP_WAIT_MS = 20_000;
const TAIL_LINES = 20;
/// Longest tool name providers accept.
const MAX_NAME = 64;
const MAX_OUTPUT = 50_000;

const home = process.env.AUGUST_HOME || join(homedir(), ".august");
const configPath = join(home, "mcp.json");

/** `mcp_<server>_<tool>` in the charset providers accept, capped in length. */
const toolName = (server: string, tool: string) =>
  `mcp_${server}_${tool}`.replace(/[^A-Za-z0-9_-]/g, "_").slice(0, MAX_NAME);

const rpcError = (msg: any) => msg.error?.message ?? JSON.stringify(msg.error);

function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: Timer;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`\`${what}\` timed out after ${ms / 1000} s`)), ms);
  });
  return Promise.race([p, timeout]).finally(() => clearTimeout(timer));
}

interface Link {
  request(method: string, params: object, ms: number): Promise<any>;
  notify(method: string): Promise<void>;
  /** Why the server is gone, once it is. */
  exited: string | null;
  onExit?: () => void;
}

function stdioLink(name: string, cfg: ServerConfig): Link {
  let proc: ReturnType<typeof Bun.spawn>;
  try {
    proc = Bun.spawn([cfg.command!, ...(cfg.args ?? [])], {
      env: { ...process.env, ...cfg.env },
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
  } catch (e) {
    throw new Error(`could not run ${cfg.command}: ${e instanceof Error ? e.message : e}`);
  }
  const waiting = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  const tail: string[] = [];
  let nextId = 1;
  const write = (msg: object) => {
    proc.stdin!.write(JSON.stringify(msg) + "\n");
    proc.stdin!.flush();
  };

  async function* lines(stream: ReadableStream<Uint8Array>) {
    const decoder = new TextDecoder();
    let buf = "";
    for await (const chunk of stream) {
      buf += decoder.decode(chunk, { stream: true });
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        yield buf.slice(0, i);
        buf = buf.slice(i + 1);
      }
    }
    if (buf) yield buf;
  }

  const stderrDone = (async () => {
    for await (const line of lines(proc.stderr as ReadableStream<Uint8Array>)) {
      console.error(`${name}: ${line}`);
      tail.push(line);
      if (tail.length > TAIL_LINES) tail.shift();
    }
  })();

  const link: Link = {
    exited: null,
    request(method, params, ms) {
      if (link.exited) return Promise.reject(new Error(link.exited));
      const id = nextId++;
      const reply = new Promise<any>((resolve, reject) => waiting.set(id, { resolve, reject }));
      try {
        write({ jsonrpc: "2.0", id, method, params });
      } catch {
        waiting.delete(id);
        return Promise.reject(new Error("the server exited"));
      }
      return withTimeout(reply, ms, method).finally(() => waiting.delete(id));
    },
    async notify(method) {
      write({ jsonrpc: "2.0", method });
    },
  };

  (async () => {
    for await (const line of lines(proc.stdout as ReadableStream<Uint8Array>)) {
      let msg: any;
      try {
        msg = JSON.parse(line);
      } catch {
        console.error(`${name}: ${line}`);
        continue;
      }
      if (msg.method !== undefined) {
        // A request from the server: answer pings, decline the rest; ignore notifications.
        if (msg.id === undefined) continue;
        write(
          msg.method === "ping"
            ? { jsonrpc: "2.0", id: msg.id, result: {} }
            : { jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "method not found" } },
        );
        continue;
      }
      const w = waiting.get(msg.id);
      if (!w) continue;
      waiting.delete(msg.id);
      if (msg.error !== undefined) w.reject(new Error(rpcError(msg)));
      else w.resolve(msg.result);
    }
    await withTimeout(stderrDone, 1000, "stderr").catch(() => {});
    link.exited = tail.length ? `the server exited: ${tail.join("\n")}` : "the server exited";
    console.error(`${name}: exited`);
    for (const w of waiting.values()) w.reject(new Error(link.exited));
    waiting.clear();
    link.onExit?.();
  })();
  return link;
}

/** Messages in an SSE body. */
function sseMessages(body: string): any[] {
  return body
    .split(/\r?\n\r?\n/)
    .map((event) =>
      event
        .split(/\r?\n/)
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).replace(/^ /, ""))
        .join("\n"),
    )
    .flatMap((data) => {
      try {
        return data ? [JSON.parse(data)] : [];
      } catch {
        return [];
      }
    });
}

function httpLink(cfg: ServerConfig): Link {
  let session: string | null = null;
  let nextId = 1;

  /** POSTs one message; returns the response to it (`null` for notifications). */
  async function post(msg: any, ms: number): Promise<any> {
    const headers: Record<string, string> = {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      "mcp-protocol-version": PROTOCOL,
      ...cfg.headers,
    };
    if (session) headers["mcp-session-id"] = session;
    const resp = await fetch(cfg.url!, { method: "POST", headers, body: JSON.stringify(msg), signal: AbortSignal.timeout(ms) });
    session = resp.headers.get("mcp-session-id") ?? session;
    // ponytail: reads the whole stream; fine while servers close it after the reply.
    const body = await resp.text();
    if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${body.trim()}`);
    if (msg.id === undefined) return null;
    const reply = (resp.headers.get("content-type") ?? "").startsWith("text/event-stream")
      ? sseMessages(body).find((m) => m.id === msg.id)
      : JSON.parse(body);
    if (!reply) throw new Error("no response from the server");
    return reply;
  }

  return {
    exited: null,
    async request(method, params, ms) {
      const reply = await post({ jsonrpc: "2.0", id: nextId++, method, params }, ms).catch((e) => {
        throw e?.name === "TimeoutError" ? new Error(`\`${method}\` timed out after ${ms / 1000} s`) : e;
      });
      if (reply.error !== undefined) throw new Error(rpcError(reply));
      return reply.result;
    },
    async notify(method) {
      await post({ jsonrpc: "2.0", method }, START_TIMEOUT_MS);
    },
  };
}

/** Text of a `tools/call` result; non-text blocks are only named. `isError` throws. */
function render(result: any): string {
  let text = (result?.content ?? [])
    .map((b: any) => {
      const uri = b.resource?.uri ?? b.uri ?? "?";
      switch (b.type) {
        case "text":
          return b.text ?? "";
        case "resource":
          return b.resource?.text ?? `[resource ${uri}]`;
        case "resource_link":
          return `[resource link ${uri}]`;
        default:
          return `[${b.type ?? "unknown"} content (${b.mimeType ?? "unknown type"}) not shown]`;
      }
    })
    .join("\n");
  if (!text && result?.structuredContent != null) text = JSON.stringify(result.structuredContent);
  if (text.length > MAX_OUTPUT) text = text.slice(0, MAX_OUTPUT) + `\n... [truncated ${text.length - MAX_OUTPUT} chars]`;
  if (result?.isError === true) throw new Error(text);
  return text;
}

type Server = { name: string; state: "connecting" | "running" | "failed"; error?: string; link?: Link; tools: string[] };

export default async function (august: August) {
  const servers: Server[] = [];

  august.registerCommand("mcp", {
    description: "List MCP servers and their tools",
    handler: () => status(),
  });

  function status(): string {
    if (!servers.length) return `No MCP servers. Configure them in \`${configPath}\`.`;
    return servers
      .map((s) => {
        if (s.state === "connecting") return `⏳ ${s.name} — connecting`;
        if (s.state === "failed") return `❌ ${s.name} — ${s.error!.trim()}`;
        if (s.link!.exited) return `❌ ${s.name} — crashed: ${s.link!.exited.trim()}`;
        return s.tools.length ? `✅ ${s.name} — tools: ${s.tools.join(", ")}` : `✅ ${s.name} — no tools`;
      })
      .join("\n");
  }

  let config: Record<string, ServerConfig>;
  try {
    const file = Bun.file(configPath);
    config = (await file.exists()) ? ((await file.json()).servers ?? {}) : {};
  } catch (e) {
    servers.push({ name: "mcp.json", state: "failed", error: e instanceof Error ? e.message : String(e), tools: [] });
    return;
  }

  // A name an earlier server took is skipped.
  const taken = new Set<string>();

  async function connect(server: Server, cfg: ServerConfig) {
    const link = cfg.command ? stdioLink(server.name, cfg) : cfg.url ? httpLink(cfg) : null;
    if (!link) throw new Error("needs a `command` or a `url`");
    server.link = link;
    const clientInfo = { name: "august", version: "0.1.0" };
    await link.request("initialize", { protocolVersion: PROTOCOL, capabilities: {}, clientInfo }, START_TIMEOUT_MS);
    await link.notify("notifications/initialized");
    const found: any[] = [];
    let cursor: string | undefined;
    do {
      const page = await link.request("tools/list", cursor ? { cursor } : {}, START_TIMEOUT_MS);
      found.push(...(page?.tools ?? []));
      cursor = typeof page?.nextCursor === "string" ? page.nextCursor : undefined;
    } while (cursor);
    if (link.exited) throw new Error(link.exited);
    for (const t of found) {
      if (typeof t.name !== "string") continue;
      const name = toolName(server.name, t.name);
      if (taken.has(name)) continue;
      taken.add(name);
      server.tools.push(name);
      august.registerTool({
        name,
        description: t.description || `Tool \`${t.name}\` of the MCP server \`${server.name}\``,
        parameters: t.inputSchema && typeof t.inputSchema === "object" ? t.inputSchema : { type: "object" },
        execute: async (input) => {
          const args = input && typeof input === "object" && !Array.isArray(input) ? input : {};
          return render(await link.request("tools/call", { name: t.name, arguments: args }, CALL_TIMEOUT_MS));
        },
      });
    }
    server.state = "running";
    link.onExit = () => server.tools.forEach((n) => august.unregisterTool(n));
  }

  const all = Object.entries(config).map(([name, cfg]) => {
    const server: Server = { name, state: "connecting", tools: [] };
    servers.push(server);
    return connect(server, cfg).catch((e) => {
      server.state = "failed";
      server.error = e instanceof Error ? e.message : String(e);
      console.error(`${name}: ${server.error}`);
    });
  });
  await Promise.race([Promise.all(all), new Promise((r) => setTimeout(r, SETUP_WAIT_MS))]);
}
