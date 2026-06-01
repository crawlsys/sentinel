import { describe, it, expect } from "vitest";

import { sentinelEventPhrase } from "../../components/EventTicker";

/// Operator-facing translation of the bridge's lifecycle event names.
/// Internal jargon (PreToolUse, etc.) leaks into the UI when this map
/// gets out of sync — keep the table tight and let unknown events
/// fall through unchanged so we never silently swallow a new
/// lifecycle event.

describe("sentinelEventPhrase", () => {
  it.each([
    ["PreToolUse", "about to run"],
    ["PostToolUse", "finished"],
    ["UserPromptSubmit", "you submitted"],
    ["Stop", "stopped"],
    ["Notification", "notified"],
    ["SubagentStop", "subagent stopped"],
    ["PreCompact", "compacting"],
  ])("translates %s -> %s", (raw, friendly) => {
    expect(sentinelEventPhrase(raw)).toBe(friendly);
  });

  it("falls through unchanged for unknown lifecycle events", () => {
    expect(sentinelEventPhrase("SomeFutureEvent")).toBe("SomeFutureEvent");
  });

  it("returns 'event' for empty string so the UI never shows a blank line", () => {
    expect(sentinelEventPhrase("")).toBe("event");
  });
});
