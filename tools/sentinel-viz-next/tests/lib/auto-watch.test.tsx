import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, fireEvent, renderHook } from "@testing-library/react";

import { AUTO_WATCH_DISABLED, useAutoWatch } from "../../hooks/auto-watch";

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useAutoWatch disabled demo mode", () => {
  it("is disabled by default for the demo", () => {
    expect(AUTO_WATCH_DISABLED).toBe(true);
  });

  it("stays off even when callers request it on", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    expect(result.current.on).toBe(false);
    expect(result.current.reason).toBe("operator");
  });

  it("does not re-enable on blur or idle", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => fireEvent.blur(window));
    expect(result.current.on).toBe(false);
    act(() => vi.advanceTimersByTime(10 * 60 * 1000 + 1_000));
    expect(result.current.on).toBe(false);
  });
});
