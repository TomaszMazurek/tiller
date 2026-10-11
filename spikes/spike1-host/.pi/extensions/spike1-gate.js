// spike1-gate: pi extension that blocks tool calls matching the spike1 test rule.
//
// Loaded from the project `.pi/extensions/` directory (requires project trust —
// the host spawns pi with `-a` so trust is granted for the process only).
//
// Rule: any `bash`/`powershell` tool call whose command contains the string
// `spike1-deny-me` is blocked with a visible reason. Every block is also
// appended to `.pi-run/gate-blocks.jsonl` as hard evidence.

import { appendFileSync, mkdirSync } from "node:fs";

const DENY_MARKER = "spike1-deny-me";

export default function spike1Gate(pi) {
  pi.on("tool_call", async (event) => {
    const command = event?.input?.command;
    if (typeof command === "string" && command.includes(DENY_MARKER)) {
      const reason =
        `[spike1-gate] blocked by spike1 test rule: command contains '${DENY_MARKER}'`;
      try {
        mkdirSync(".pi-run", { recursive: true });
        appendFileSync(
          ".pi-run/gate-blocks.jsonl",
          JSON.stringify({
            ts: new Date().toISOString(),
            tool: event.toolName,
            toolCallId: event.toolCallId,
            blocked: true,
            reason,
          }) + "\n",
        );
      } catch {
        // evidence log is best-effort; the block itself must still happen
      }
      return { block: true, reason };
    }
    return undefined;
  });
}
