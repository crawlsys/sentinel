import { describe, it, expect } from "vitest";

import {
  friendlyTitle,
  shouldShowRawAwaiting,
} from "../../components/PanelInspector";
import type { Node } from "../../types/api";

function sessionNode(opts: { sid?: string; name?: string } = {}): Node {
  return {
    id: "SentinelSession#z",
    type: "SentinelSession",
    data: {
      ...(opts.sid ? { session_id: opts.sid } : {}),
      ...(opts.name ? { name: opts.name } : {}),
    },
    ts: "2026-05-26T00:00:00Z",
    seq: 1,
  };
}

describe("PanelInspector friendlyTitle", () => {
  it("uses the LLM-assigned name AND short sid when both are present", () => {
    expect(
      friendlyTitle(sessionNode({ sid: "abcdef1234567890", name: "warm-otter" })),
    ).toBe("warm-otter · s:abcdef12");
  });

  it("falls back to 'session · s:<short>' when name is missing", () => {
    expect(friendlyTitle(sessionNode({ sid: "abcdef1234567890" }))).toBe("session · s:abcdef12");
  });

  it("falls back to the name alone if sid is missing", () => {
    expect(friendlyTitle(sessionNode({ name: "warm-otter" }))).toBe("warm-otter");
  });

  it("never returns the anonymous 'session' label when a sid is available", () => {
    const out = friendlyTitle(sessionNode({ sid: "abcdef12" }));
    expect(out).not.toBe("session");
  });

  it("still works for tool-call and hook nodes", () => {
    const toolNode: Node = {
      id: "SentinelToolCall#1",
      type: "SentinelToolCall",
      data: { tool: "Bash" },
      ts: "",
      seq: 1,
    };
    expect(friendlyTitle(toolNode)).toBe("tool · Bash");

    const hookNode: Node = {
      id: "SentinelHookInvocation#1",
      type: "SentinelHookInvocation",
      data: { hook: "tool_usage_gate" },
      ts: "",
      seq: 1,
    };
    expect(friendlyTitle(hookNode)).toBe("hook · tool_usage_gate");
  });
});

describe("shouldShowRawAwaiting (dedup vs SummaryCard)", () => {
  it("always shows the raw block in 'card' mode (we aren't waiting yet)", () => {
    expect(shouldShowRawAwaiting("card", "summary text")).toBe(true);
    expect(shouldShowRawAwaiting("card", null)).toBe(true);
  });

  it("HIDES the raw block when wait-summary has loaded text (LLM rollup covers it)", () => {
    expect(shouldShowRawAwaiting("wait", "operator-facing summary")).toBe(false);
  });

  it("SHOWS the raw block when wait-summary is null / empty / whitespace (LLM disabled or pending)", () => {
    expect(shouldShowRawAwaiting("wait", null)).toBe(true);
    expect(shouldShowRawAwaiting("wait", "")).toBe(true);
    expect(shouldShowRawAwaiting("wait", "   \n\t")).toBe(true);
  });
});
