// `/goal <text>`: after every turn a separate judging call checks the goal; while it isn't
// reached the chat keeps working on it, up to AUGUST_GOAL_TURNS turns (default 20).
// Goals live in memory: a restart, /goal clear, /new or /stop drops them.

import type { August, Chat } from "august";

const JUDGE =
  "You check whether an AI assistant has reached a goal the user set. You get the goal and " +
  "the assistant's latest reply. Answer `DONE` if the reply shows the goal is fully achieved " +
  "(or can't be achieved and the assistant explained why), otherwise `CONTINUE: ` and one " +
  "sentence on what is still missing. Output only that line.";

type Goal = { text: string; turns: number };

export default function (august: August) {
  const maxTurns = Number(process.env.AUGUST_GOAL_TURNS) || 20;
  const goals = new Map<string, Goal>();
  const key = (chat: Chat | null) => (chat ? `${chat.channel}:${chat.chat}` : "cli");

  august.registerCommand("goal", {
    description: "Keep working until a goal is reached (/goal clear to stop)",
    async handler(args, ctx) {
      const k = key(ctx.chat);
      const goal = goals.get(k);
      if (!args) {
        return goal
          ? `🎯 Goal: ${goal.text} (${goal.turns} turns so far). /goal clear to drop it.`
          : "No goal. Set one with /goal <what should be achieved>.";
      }
      if (args === "clear") return goals.delete(k) ? "Goal dropped." : "No goal to drop.";
      goals.set(k, { text: args, turns: 0 });
      await ctx.prompt(`[New goal] ${args}\nWork on it until it is achieved; I'll check after each turn.`);
      return `🎯 Goal set: ${args}`;
    },
  });

  // Otherwise the goal's loop would start the next turn.
  august.on("stop", async (_, ctx) => {
    if (goals.delete(key(ctx.chat))) await ctx.send("Goal dropped.");
  });
  august.on("session_start", (_, ctx) => {
    goals.delete(key(ctx.chat));
  });

  august.on("turn_end", async ({ reply, unattended }, ctx) => {
    const k = key(ctx.chat);
    const goal = goals.get(k);
    if (!goal || unattended) return;
    let verdict: string;
    try {
      verdict = (await ctx.llm(`Goal:\n${goal.text}\n\nThe assistant's latest reply:\n${reply}`, { system: JUDGE })).trim();
    } catch (e) {
      goals.delete(k);
      return ctx.send(`⚠️ Could not check the goal, pausing it: ${e instanceof Error ? e.message : e}`);
    }
    if (goals.get(k) !== goal) return; // dropped or replaced while judging
    if (verdict.startsWith("DONE")) {
      goals.delete(k);
      return ctx.send(`🎯 Goal reached: ${goal.text}`);
    }
    const missing = verdict.replace(/^CONTINUE/, "").replace(/^[:\s]+/, "").trim() || "the goal isn't reached yet";
    goal.turns++;
    if (goal.turns >= maxTurns) {
      goals.delete(k);
      return ctx.send(`⏸ Goal paused after ${goal.turns} turns. Still missing: ${missing}\nSet it again with /goal to continue.`);
    }
    await ctx.prompt(`[Goal not reached yet: ${missing}] Keep working towards the goal: ${goal.text}`);
  });
}
