// `clarify`: the agent asks the user a question with a few answers to tap, and waits.

import type { August } from "august";

export default function (august: August) {
  august.registerTool<{ question: string; options: string[] }>({
    name: "clarify",
    description:
      "Ask the user a question with 2-6 short answers to choose from (buttons in the chat) and " +
      "wait for the choice. Use it when a decision is genuinely theirs and the options are " +
      "clear; for open questions just ask in your reply.",
    parameters: {
      type: "object",
      properties: {
        question: { type: "string" },
        options: { type: "array", items: { type: "string" }, minItems: 2, maxItems: 6 },
      },
      required: ["question", "options"],
      additionalProperties: false,
    },
    async execute({ question, options }, ctx) {
      if (!question?.trim() || !Array.isArray(options) || options.length < 2) throw new Error("need a question and at least 2 options");
      const answer = await ctx.ask(question, options.slice(0, 6).map(String));
      return answer === null ? "The user didn't answer within 5 minutes." : `The user chose: ${answer}`;
    },
  });
}
