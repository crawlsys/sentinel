/// Tool-set membership tables. Moved here from EventTicker so any
/// component that needs to bucket events into operator-meaningful
/// categories (ticker, session strips, kpi bar) shares one source
/// of truth. Add new tools to the right set; everything else falls
/// through to "other".

import type { NodeCategory } from "../types/api";

export const TC_TOOLS = new Set([
  "Bash",
  "Read",
  "Write",
  "Edit",
  "Grep",
  "Glob",
  "NotebookEdit",
  "MultiEdit",
]);

export const PLANNING_TOOLS = new Set([
  "TaskCreate",
  "TaskUpdate",
  "TaskList",
  "TaskGet",
  "TaskStop",
  "TaskOutput",
  "WebFetch",
  "WebSearch",
  "Plan",
  "ExitPlanMode",
  "EnterPlanMode",
]);

export const COMMUNICATION_TOOLS = new Set([
  "Agent",
  "AskUserQuestion",
  "Stop",
  "ToolSearch",
]);

/// Map a (sentinel_event, tool) pair into the operator-facing
/// category bucket. UserPromptSubmit always wins → "prompt".
/// Tools fall into tc / planning / communication / other.
export function categoryForTool(sentinelEvent: string, tool: string | null): NodeCategory {
  if (sentinelEvent === "UserPromptSubmit") return "prompt";
  if (tool && TC_TOOLS.has(tool)) return "tc";
  if (tool && PLANNING_TOOLS.has(tool)) return "planning";
  if (tool && COMMUNICATION_TOOLS.has(tool)) return "communication";
  return "other";
}

/// Derive label + category for a single event payload. Mirrors
/// what EventTicker.buildRows() needs; centralised so both stay in
/// sync. Hooks (label = hook name, category = other) win over the
/// lifecycle-event fallback when there's no tool.
export function deriveLabelAndCategory(
  evType: string,
  sentinelEvent: string,
  tool: string | null,
  hook: string | null = null,
): { label: string; category: NodeCategory } {
  if (sentinelEvent === "UserPromptSubmit") {
    return { label: "user prompt", category: "prompt" };
  }
  if (tool && tool.length > 0) {
    return { label: tool, category: categoryForTool(sentinelEvent, tool) };
  }
  if (hook && hook.length > 0) return { label: hook, category: "other" };
  return { label: sentinelEvent || evType.replace(/^sentinel\./, ""), category: "other" };
}
