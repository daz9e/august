// `delegate_task`: hands a piece of work to a background sub-agent with a fresh context;
// its report comes back to the chat as a new message (joining the running turn if any).

import type { August } from "august";

const SURFACE =
  "You are a sub-agent doing one task for the main agent, which talks to the user. You see " +
  "only the task below, not their conversation, and nobody can answer questions: work " +
  "autonomously and make reasonable assumptions. Finish with a concise report for the main " +
  "agent: what you did, what you found, files you changed, and anything left open or uncertain.";

// What a sub-agent may not do: talk to the user, change memory or skills, schedule or
// delegate more work.
const EXCLUDE = [
  "delegate_task", "schedule_task", "list_tasks", "cancel_task", "remember", "forget", "send_file",
  "save_skill", "edit_skill", "save_extension",
];

export default function (august: August) {
  let count = 0;

  august.registerTool<{ goal: string; context?: string }>({
    name: "delegate_task",
    description:
      "Hand a self-contained piece of work to a sub-agent that runs in the background with a " +
      "fresh context and the same tools (research, a long build or investigation, several " +
      "independent parts at once — call it once per part). The sub-agent sees nothing of this " +
      "conversation: put everything it needs into `goal` and `context` (paths, names, " +
      "constraints, what a good result looks like). Its report arrives later as a new message; " +
      "meanwhile you can keep talking to the user.",
    parameters: {
      type: "object",
      properties: {
        goal: { type: "string", description: "What to achieve, in one or two sentences" },
        context: { type: "string", description: "Everything the sub-agent needs to know" },
      },
      required: ["goal"],
      additionalProperties: false,
    },
    execute({ goal, context }, ctx) {
      goal = (goal ?? "").trim();
      if (!goal) throw new Error("empty goal");
      const n = ++count;
      const title = goal.slice(0, 80);
      const task = context?.trim() ? `${goal}\n\nContext:\n${context}` : goal;
      // ponytail: sub-agents are not cancelled by /stop and not capped in number; add both if they get used heavily.
      ctx.agent(task, { system: SURFACE, exclude: EXCLUDE }).then(
        (report) => ctx.prompt(`[Subtask #${n} finished: ${title}]\n${report}`),
        (e) => ctx.prompt(`[Subtask #${n} failed: ${title}]\n${e instanceof Error ? e.message : e}`),
      );
      return `Started subtask #${n}. Its report will arrive as a new message; carry on with other work or end your turn.`;
    },
  });
}
