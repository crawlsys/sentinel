import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, fireEvent, render, renderHook, screen } from "@testing-library/react";

import { useAutoWatch, AUTO_WATCH_IGNORE_ATTR } from "../../lib/auto-watch";

/// These tests pin down the exact behaviors the operator complained
/// about — particularly the bug where the toggle would flip back to
/// OFF the instant the cursor moved, because the original
/// implementation listened to mousemove at capture phase. Each test
/// here corresponds to a contract the toggle UI relies on.

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useAutoWatch", () => {
  it("starts off by default and reports operator as the reason", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    expect(result.current.on).toBe(false);
    expect(result.current.reason).toBe("operator");
  });

  it("set(true) flips state and survives subsequent mousemove (regression: mousemove no longer counted)", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    expect(result.current.on).toBe(true);

    // Move the mouse a bunch — this is what was killing the toggle.
    act(() => {
      for (let i = 0; i < 20; i++) {
        fireEvent.mouseMove(document, { clientX: i, clientY: i });
      }
    });
    expect(result.current.on).toBe(true);
  });

  it("set(true) survives scroll events too — they used to count as interaction", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    act(() => {
      for (let i = 0; i < 5; i++) fireEvent.scroll(document);
    });
    expect(result.current.on).toBe(true);
  });

  it("click anywhere in the document disables auto when on", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));

    // Advance past the post-toggle grace window so the next click
    // is actually counted as interaction.
    act(() => vi.advanceTimersByTime(800));

    act(() => fireEvent.click(document.body));
    expect(result.current.on).toBe(false);
    expect(result.current.reason).toBe("interaction");
  });

  it("keydown disables auto when on", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    act(() => vi.advanceTimersByTime(800));

    act(() => fireEvent.keyDown(document, { key: "a" }));
    expect(result.current.on).toBe(false);
    expect(result.current.reason).toBe("interaction");
  });

  it("clicks inside an element marked with data-auto-watch-ignore are ignored", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    act(() => vi.advanceTimersByTime(800));

    const btn = document.createElement("button");
    btn.setAttribute(AUTO_WATCH_IGNORE_ATTR, "");
    document.body.appendChild(btn);
    try {
      act(() => fireEvent.click(btn));
      expect(result.current.on).toBe(true);
    } finally {
      btn.remove();
    }
  });

  it("clicks inside a CHILD of an ignore-marked element are also ignored", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    act(() => vi.advanceTimersByTime(800));

    const wrap = document.createElement("div");
    wrap.setAttribute(AUTO_WATCH_IGNORE_ATTR, "");
    const inner = document.createElement("span");
    wrap.appendChild(inner);
    document.body.appendChild(wrap);
    try {
      act(() => fireEvent.click(inner));
      expect(result.current.on).toBe(true);
    } finally {
      wrap.remove();
    }
  });

  it("post-set grace swallows the bubbling click that triggered the toggle", () => {
    // Simulates the realistic toggle flow: operator clicks AUTO,
    // set(true) runs, then the SAME click bubbles up to document.
    // Without grace, that click would immediately set(false).
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    // No timer advance — we're inside the grace window.
    act(() => fireEvent.click(document.body));
    expect(result.current.on).toBe(true);
  });

  it("window blur turns auto ON when it was off", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    expect(result.current.on).toBe(false);
    act(() => fireEvent.blur(window));
    expect(result.current.on).toBe(true);
    expect(result.current.reason).toBe("blur");
  });

  it("window blur is a no-op when auto is already on", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    act(() => result.current.set(true));
    const reasonBefore = result.current.reason;
    act(() => fireEvent.blur(window));
    expect(result.current.on).toBe(true);
    expect(result.current.reason).toBe(reasonBefore);
  });

  it("10 minutes of no interaction flips auto on", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    expect(result.current.on).toBe(false);
    act(() => vi.advanceTimersByTime(10 * 60 * 1000 + 1_000));
    expect(result.current.on).toBe(true);
    expect(result.current.reason).toBe("idle");
  });

  it("interaction resets the idle clock so we don't auto-enable mid-work", () => {
    const { result } = renderHook(() => useAutoWatch(false));
    // 5min idle, then a click resets the clock.
    act(() => vi.advanceTimersByTime(5 * 60 * 1000));
    act(() => fireEvent.click(document.body));
    // Another 6min — total 11min wall-clock, but only 6min since last
    // interaction. Should NOT have flipped to idle.
    act(() => vi.advanceTimersByTime(6 * 60 * 1000));
    expect(result.current.on).toBe(false);
    // Another 5min — now we're 11min since last interaction. Idle fires.
    act(() => vi.advanceTimersByTime(5 * 60 * 1000));
    expect(result.current.on).toBe(true);
    expect(result.current.reason).toBe("idle");
  });
});

/// End-to-end test against a tiny harness component that wires up
/// useAutoWatch to a button — this catches the exact "click button,
/// then move mouse, toggle dies" failure the user reported.
describe("useAutoWatch end-to-end with a toggle button", () => {
  function Harness() {
    const auto = useAutoWatch(false);
    return (
      <>
        <button
          data-testid="toggle"
          {...{ [AUTO_WATCH_IGNORE_ATTR]: "" }}
          onClick={() => auto.set(!auto.on)}
        >
          AUTO {auto.on ? "ON" : "OFF"}
        </button>
        <span data-testid="state">{auto.on ? "on" : "off"}</span>
      </>
    );
  }

  it("clicking the toggle flips state and stays flipped through mousemove", () => {
    render(<Harness />);
    expect(screen.getByTestId("state").textContent).toBe("off");
    fireEvent.click(screen.getByTestId("toggle"));
    expect(screen.getByTestId("state").textContent).toBe("on");
    for (let i = 0; i < 10; i++) fireEvent.mouseMove(document);
    expect(screen.getByTestId("state").textContent).toBe("on");
  });

  it("clicking the toggle a second time flips back to off", () => {
    render(<Harness />);
    fireEvent.click(screen.getByTestId("toggle"));
    expect(screen.getByTestId("state").textContent).toBe("on");
    act(() => vi.advanceTimersByTime(800));
    fireEvent.click(screen.getByTestId("toggle"));
    expect(screen.getByTestId("state").textContent).toBe("off");
  });

  it("clicking somewhere NOT in the toggle disables auto", () => {
    render(
      <>
        <Harness />
        <div data-testid="canvas">graph</div>
      </>,
    );
    fireEvent.click(screen.getByTestId("toggle"));
    expect(screen.getByTestId("state").textContent).toBe("on");
    act(() => vi.advanceTimersByTime(800));
    fireEvent.click(screen.getByTestId("canvas"));
    expect(screen.getByTestId("state").textContent).toBe("off");
  });
});
