import { beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../../adapters/http";
import { _reset, peek, request, subscribe } from "../../adapters/rollup-summary-cache";

vi.mock("../../adapters/http", () => ({
  fetchRollupSummary: vi.fn(),
}));

beforeEach(() => {
  _reset();
  vi.mocked(api.fetchRollupSummary).mockReset();
});

describe("rollup-summary-cache", () => {
  it("peek returns undefined for never-requested keys", () => {
    expect(peek("k-1")).toBeUndefined();
  });

  it("request fires fetch on first call, caches on success, notifies subscribers", async () => {
    vi.mocked(api.fetchRollupSummary).mockResolvedValueOnce({
      cache_key: "k-1",
      session_id: "sess-a",
      summary: "edited EventTicker, ran tests, pushed",
      source: "llm:openai/gpt-4o-mini",
      cached: false,
    });
    const notifications: number[] = [];
    const unsub = subscribe(() => notifications.push(Date.now()));

    const resp = await request("k-1", "sess-a", [{ tool: "Bash", summary: "git push" }]);
    expect(resp.summary).toBe("edited EventTicker, ran tests, pushed");
    expect(api.fetchRollupSummary).toHaveBeenCalledOnce();
    expect(notifications.length).toBe(1);

    // peek returns the cached entry now.
    expect(peek("k-1")).toEqual({
      summary: "edited EventTicker, ran tests, pushed",
      source: "llm:openai/gpt-4o-mini",
    });

    unsub();
  });

  it("concurrent callers with the same key share one in-flight fetch", async () => {
    let resolveFetch: ((v: api.RollupSummaryResponse) => void) | null = null;
    vi.mocked(api.fetchRollupSummary).mockImplementationOnce(
      () => new Promise((res) => { resolveFetch = res; }),
    );

    const a = request("k-1", "sess-a", []);
    const b = request("k-1", "sess-a", []);
    expect(api.fetchRollupSummary).toHaveBeenCalledOnce();

    resolveFetch!({
      cache_key: "k-1", session_id: "sess-a",
      summary: "x", source: "llm:fake", cached: false,
    });
    const [ra, rb] = await Promise.all([a, b]);
    expect(ra.summary).toBe("x");
    expect(rb.summary).toBe("x");
  });

  it("re-request after cache hit returns immediately without fetch", async () => {
    vi.mocked(api.fetchRollupSummary).mockResolvedValueOnce({
      cache_key: "k-1", session_id: "sess-a",
      summary: "done", source: "llm:x", cached: false,
    });
    await request("k-1", "sess-a", []);
    expect(api.fetchRollupSummary).toHaveBeenCalledOnce();

    const cached = await request("k-1", "sess-a", []);
    expect(cached.cached).toBe(true);
    expect(cached.summary).toBe("done");
    expect(api.fetchRollupSummary).toHaveBeenCalledOnce(); // no second fetch
  });

  it("server-side null summary (no-model / llm-error) is cached so we stop hammering", async () => {
    vi.mocked(api.fetchRollupSummary).mockResolvedValueOnce({
      cache_key: "k-1", session_id: "sess-a",
      summary: null, source: "llm-error", cached: false,
    });
    await request("k-1", "sess-a", []);
    expect(peek("k-1")).toEqual({ summary: null, source: "llm-error" });
    // Subsequent calls don't re-fetch.
    await request("k-1", "sess-a", []);
    expect(api.fetchRollupSummary).toHaveBeenCalledOnce();
  });

  it("network failure does NOT cache — next attempt can succeed", async () => {
    vi.mocked(api.fetchRollupSummary).mockRejectedValueOnce(new Error("network"));
    await expect(request("k-1", "sess-a", [])).rejects.toThrow("network");
    expect(peek("k-1")).toBeUndefined();

    vi.mocked(api.fetchRollupSummary).mockResolvedValueOnce({
      cache_key: "k-1", session_id: "sess-a",
      summary: "recovered", source: "llm:x", cached: false,
    });
    const r = await request("k-1", "sess-a", []);
    expect(r.summary).toBe("recovered");
    expect(api.fetchRollupSummary).toHaveBeenCalledTimes(2);
  });
});
