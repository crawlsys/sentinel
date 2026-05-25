import { describe, it, expect } from "vitest";

import { nodeColor, relTime, shortTime, statusColor } from "../../lib/format";

describe("format helpers", () => {
  it("relTime under one minute", () => {
    const now = new Date("2026-05-25T00:00:30Z").getTime();
    expect(relTime("2026-05-25T00:00:00Z", now)).toBe("30s ago");
  });

  it("relTime over an hour", () => {
    const now = new Date("2026-05-25T02:00:00Z").getTime();
    expect(relTime("2026-05-25T00:00:00Z", now)).toBe("2h ago");
  });

  it("shortTime extracts HH:MM:SS", () => {
    expect(shortTime("2026-05-25T13:45:09.123Z")).toBe("13:45:09");
  });

  it("statusColor returns muted grey for unknown", () => {
    expect(statusColor()).toBe("#6e7681");
    expect(statusColor("firing")).toBe("#3fb950");
    expect(statusColor("awaiting_user")).toBe("#bc8cff");
  });

  it("nodeColor reflects denied outcome", () => {
    expect(nodeColor("SentinelHookInvocation", "denied")).toBe("#f85149");
    expect(nodeColor("SentinelSession")).toBe("#bc8cff");
    expect(nodeColor("Other")).toBe("#6e7681");
  });
});
