import { describe, it, expect } from "vitest";

import { awaitingKindLabel, nodeColor, relTime, shortTime, statusColor, statusLabel, tickerTime } from "../../domain/format";

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

  describe("statusLabel translates raw enum jargon to operator words", () => {
    it.each([
      ["firing", "active"],
      ["busy", "busy"],
      ["idle", "idle"],
      ["dormant", "dormant"],
      ["dead", "dead"],
      ["awaiting_user", "waiting"],
    ])("maps %s -> %s", (raw, friendly) => {
      expect(statusLabel(raw)).toBe(friendly);
    });

    it("never leaks the underscored 'awaiting_user' enum", () => {
      expect(statusLabel("awaiting_user")).not.toContain("_");
    });

    it("returns a dash for null/undefined (matches the strip's empty fallback)", () => {
      expect(statusLabel()).toBe("—");
      expect(statusLabel(null)).toBe("—");
    });

    it("falls through unchanged for unknown statuses (never swallow a novel one)", () => {
      expect(statusLabel("some_future_status")).toBe("some_future_status");
    });
  });

  describe("awaitingKindLabel translates the stuck-reason kind to operator phrasing", () => {
    it.each([
      ["AskUserQuestion", "your answer"],
      ["question", "your answer"],
      ["reply", "your reply"],
      ["PreToolUse", "tool approval"],
      ["Stop", "stop confirmation"],
      ["Notification", "your attention"],
    ])("maps %s -> %s", (raw, friendly) => {
      expect(awaitingKindLabel(raw)).toBe(friendly);
    });

    it("never leaks the camel-case 'AskUserQuestion' identifier", () => {
      expect(awaitingKindLabel("AskUserQuestion")).not.toMatch(/AskUserQuestion/);
    });

    it("returns 'awaiting' for null/undefined (matches the banner's inline fallback)", () => {
      expect(awaitingKindLabel()).toBe("awaiting");
      expect(awaitingKindLabel(null)).toBe("awaiting");
    });

    it("falls through unchanged for an unknown kind (never swallow a novel one)", () => {
      expect(awaitingKindLabel("SomeFutureKind")).toBe("SomeFutureKind");
    });
  });

  describe("tickerTime buckets", () => {
    // Pick a fixed "now" anchor so the relative buckets are deterministic.
    const NOW = new Date("2026-05-25T12:00:00Z").getTime();
    const at = (offsetSec: number) =>
      new Date(NOW - offsetSec * 1000).toISOString();

    it('< 5s reads "now"', () => {
      expect(tickerTime(at(2), NOW)).toBe("now");
    });

    it("< 90s reads in seconds", () => {
      expect(tickerTime(at(30), NOW)).toBe("30s ago");
      expect(tickerTime(at(89), NOW)).toBe("89s ago");
    });

    it("< 1h reads in minutes", () => {
      expect(tickerTime(at(120), NOW)).toBe("2m ago");
      expect(tickerTime(at(3599), NOW)).toBe("59m ago");
    });

    it("< 90m reads in hours and minutes", () => {
      expect(tickerTime(at(3600 + 1380), NOW)).toBe("1h 23m ago");
    });

    it(">= 90m falls back to absolute HH:MM (local TZ)", () => {
      // 95min ago — should be absolute time string with a colon
      const s = tickerTime(at(95 * 60), NOW);
      expect(s).toMatch(/^\d{2}:\d{2}$/);
    });

    it(">= 24h prepends a weekday short name", () => {
      // 26h ago
      const s = tickerTime(at(26 * 3600), NOW);
      expect(s).toMatch(/^(Mon|Tue|Wed|Thu|Fri|Sat|Sun) \d{2}:\d{2}$/);
    });

    it("handles bridge ts_sec (no Z suffix) as UTC", () => {
      const noZ = "2026-05-25T11:58:00";
      // 2 minutes before noon UTC anchor → "2m ago"
      expect(tickerTime(noZ, NOW)).toBe("2m ago");
    });

    it("returns dash on unparseable input", () => {
      expect(tickerTime("nope", NOW)).toBe("—");
    });
  });

  it("nodeColor reflects denied outcome", () => {
    expect(nodeColor("SentinelHookInvocation", "denied")).toBe("#f85149");
    expect(nodeColor("SentinelSession")).toBe("#bc8cff");
    expect(nodeColor("Other")).toBe("#6e7681");
  });
});
