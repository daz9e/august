// Voice notes and audio files are transcribed for the agent via an OpenAI-compatible
// `POST {base}/audio/transcriptions`: `AUGUST_TRANSCRIBE_URL` (e.g. Groq) with
// `AUGUST_TRANSCRIBE_API_KEY`, else OpenAI with that key or the OpenAI provider's key
// (only when it points at OpenAI itself). `AUGUST_TRANSCRIBE_MODEL` defaults to whisper-1.

import type { August } from "august";
import { basename, join } from "node:path";
import { homedir } from "node:os";

const OPENAI_URL = "https://api.openai.com/v1";
const TIMEOUT_MS = 100_000;

const env = (key: string) => process.env[key] || undefined;

/** The OpenAI provider's key, if it talks to OpenAI itself. */
async function openaiKey(): Promise<string | undefined> {
  let stored: { key?: string; base_url?: string } | undefined;
  try {
    const home = env("AUGUST_HOME") ?? join(homedir(), ".august");
    stored = (await Bun.file(join(home, "credentials.json")).json()).openai;
  } catch {}
  const base = env("OPENAI_BASE_URL") ?? stored?.base_url ?? OPENAI_URL;
  return base === OPENAI_URL ? (env("OPENAI_API_KEY") ?? stored?.key) : undefined;
}

async function transcribe(path: string, mime: string): Promise<string> {
  let base = env("AUGUST_TRANSCRIBE_URL");
  let key = env("AUGUST_TRANSCRIBE_API_KEY");
  if (!base) {
    key ??= await openaiKey();
    if (!key) {
      throw new Error(
        "transcription is not configured (set AUGUST_TRANSCRIBE_API_KEY or OPENAI_API_KEY, " +
          "or AUGUST_TRANSCRIBE_URL for another OpenAI-compatible service)",
      );
    }
    base = OPENAI_URL;
  }
  const form = new FormData();
  form.append("model", env("AUGUST_TRANSCRIBE_MODEL") ?? "whisper-1");
  form.append("file", new File([await Bun.file(path).arrayBuffer()], basename(path), { type: mime }));
  const headers: Record<string, string> = key ? { authorization: `Bearer ${key}` } : {};
  const res = await fetch(`${base.replace(/\/+$/, "")}/audio/transcriptions`, {
    method: "POST",
    headers,
    body: form,
    signal: AbortSignal.timeout(TIMEOUT_MS),
  }).catch((e) => {
    throw new Error(`transcription request failed: ${e instanceof Error ? e.message : e}`);
  });
  const body = await res.text();
  if (!res.ok) throw new Error(`transcription failed: HTTP ${res.status}: ${body.slice(0, 300)}`);
  let text: unknown;
  try {
    text = JSON.parse(body).text;
  } catch {
    throw new Error("transcription: bad response");
  }
  if (typeof text !== "string" || !text.trim()) throw new Error("transcription returned no text");
  return text.trim();
}

export default function (august: August) {
  august.on("message_in", async ({ text, files }) => {
    const audio = (files ?? []).filter((f) => f.mime.startsWith("audio/"));
    if (!audio.length) return;
    const notes = [];
    for (const f of audio) {
      try {
        notes.push(`[${f.voice ? "Voice message" : "Audio"} transcript]\n${await transcribe(f.path, f.mime)}`);
      } catch (e) {
        notes.push(`[No transcript: ${e instanceof Error ? e.message : e}]`);
      }
    }
    return { text: [text, ...notes].filter(Boolean).join("\n") };
  });
}
