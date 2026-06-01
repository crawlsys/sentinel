import { beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../../lib/api";
import { _resetSessionNames, ensureName, getCachedName, subscribe } from "../../lib/session-names";

vi.mock("../../lib/api", () => ({
  fetchSessionName: vi.fn(),
}));

beforeEach(() => {
  _resetSessionNames();
  vi.mocked(api.fetchSessionName).mockReset();
});

describe("session-names", () => {
  it("getCachedName returns undefined for never-requested ids", () => {
    expect(getCachedName("sess-x")).toBeUndefined();
  });

  it("ensureName triggers a single fetch + notifies on completion", async () => {
    vi.mocked(api.fetchSessionName).mockResolvedValueOnce({
      session_id: "sess-a",
      name: "viz rewrite",
      source: "openai:gpt-4o-mini",
      cached: false,
    });
    const seen: number[] = [];
    const off = subscribe(() => seen.push(Date.now()));

    const initial = ensureName("sess-a");
    expect(initial).toBeUndefined();
    expect(api.fetchSessionName).toHaveBeenCalledOnce();

    // Re-call while inflight → no extra fetch.
    ensureName("sess-a");
    expect(api.fetchSessionName).toHaveBeenCalledOnce();

    // Wait for the promise microtask to settle.
    await new Promise((r) => setTimeout(r, 0));

    expect(getCachedName("sess-a")).toBe("viz rewrite");
    expect(seen).toHaveLength(1);
    off();
  });

  it("on fetch error caches null and stops re-trying", async () => {
    vi.mocked(api.fetchSessionName).mockRejectedValueOnce(new Error("nope"));
    ensureName("sess-b");
    await new Promise((r) => setTimeout(r, 0));
    expect(getCachedName("sess-b")).toBeNull();
    // Subsequent calls return cached null, no new fetch.
    ensureName("sess-b");
    expect(api.fetchSessionName).toHaveBeenCalledOnce();
  });

  it("caches different sessions independently", async () => {
    vi.mocked(api.fetchSessionName)
      .mockResolvedValueOnce({ session_id: "a", name: "x", source: "openai", cached: false })
      .mockResolvedValueOnce({ session_id: "b", name: "y", source: "openai", cached: false });
    ensureName("a");
    ensureName("b");
    await new Promise((r) => setTimeout(r, 0));
    expect(getCachedName("a")).toBe("x");
    expect(getCachedName("b")).toBe("y");
  });
});
