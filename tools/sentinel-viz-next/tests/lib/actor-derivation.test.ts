import { describe, it, expect } from "vitest";

import { deriveActor, isInterventionOutcome } from "../../components/EventTicker";

/// These rules are the contract between the bridge's event shape and
/// the operator's "who did this?" mental model. The bridge can add
/// fields freely; if the rules stop fitting, the right move is to
/// add an explicit `actor` field on the wire — not to grow this
/// derivation into a thicket.

describe("deriveActor", () => {
  describe("user-originated events", () => {
    it("UserPromptSubmit is always 'user' regardless of other fields", () => {
      expect(deriveActor("sentinel.tool_call_observed", "UserPromptSubmit", null)).toBe("user");
      expect(deriveActor("sentinel.tool_call_observed", "UserPromptSubmit", "deny")).toBe("user");
      expect(deriveActor("sentinel.hook_ingested", "UserPromptSubmit", null)).toBe("user");
    });
  });

  describe("sentinel-originated events (the control plane intervening)", () => {
    it("classifies hook-ingested events as 'sentinel'", () => {
      expect(deriveActor("sentinel.hook_ingested", "PreToolUse", null)).toBe("sentinel");
      expect(deriveActor("sentinel.hook_ingested", "PostToolUse", null)).toBe("sentinel");
    });

    it.each(["deny", "denied", "inject", "injected", "force_stop", "block", "blocked"])(
      "classifies outcome=%s as 'sentinel' even when the event type is tool_call_observed",
      (outcome) => {
        expect(deriveActor("sentinel.tool_call_observed", "PreToolUse", outcome)).toBe("sentinel");
      },
    );

    it("does NOT classify benign outcomes (allow, ok, complete) as sentinel", () => {
      expect(deriveActor("sentinel.tool_call_observed", "PreToolUse", "allow")).toBe("claude");
      expect(deriveActor("sentinel.tool_call_observed", "PreToolUse", "ok")).toBe("claude");
      expect(deriveActor("sentinel.tool_call_observed", "PreToolUse", "complete")).toBe("claude");
    });
  });

  describe("claude-originated events (the default)", () => {
    it("tool_call_observed with no outcome → 'claude'", () => {
      expect(deriveActor("sentinel.tool_call_observed", "PreToolUse", null)).toBe("claude");
      expect(deriveActor("sentinel.tool_call_observed", "PostToolUse", null)).toBe("claude");
      expect(deriveActor("sentinel.tool_call_observed", "Stop", null)).toBe("claude");
    });

    it("Unknown sentinel events default to 'claude'", () => {
      expect(deriveActor("sentinel.tool_call_observed", "SomeFutureEvent", null)).toBe("claude");
    });
  });
});

describe("isInterventionOutcome", () => {
  it("returns true for deny/inject/force_stop family", () => {
    for (const o of ["deny", "denied", "inject", "injected", "force_stop", "block", "blocked"]) {
      expect(isInterventionOutcome(o)).toBe(true);
    }
  });

  it("returns false for null / empty / benign outcomes", () => {
    expect(isInterventionOutcome(null)).toBe(false);
    expect(isInterventionOutcome("")).toBe(false);
    expect(isInterventionOutcome("allow")).toBe(false);
    expect(isInterventionOutcome("ok")).toBe(false);
    expect(isInterventionOutcome("complete")).toBe(false);
  });

  it("is case-sensitive on purpose — bridge writes lower-case outcomes", () => {
    expect(isInterventionOutcome("DENY")).toBe(false);
    expect(isInterventionOutcome("Deny")).toBe(false);
  });
});
