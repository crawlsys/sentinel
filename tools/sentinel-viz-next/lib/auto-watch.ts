"use client";

import { useCallback, useEffect, useRef, useState } from "react";

/// Auto-watch mode. When ON, the page auto-focuses the freshest event
/// on every SSE tick. When OFF, manual selection only.
///
/// Auto-off triggers (deliberate operator activity):
///   - click anywhere in the document EXCEPT the toggle button itself
///   - keydown
/// Notably we do NOT count mousemove or scroll — they fire on every
/// pixel of cursor drift and would flip the toggle back to OFF the
/// instant the operator finished clicking it ON.
///
/// Auto-on triggers (operator clearly not looking):
///   - window blur (operator switched tabs/apps)
///   - 10 minutes of no qualifying interaction
///
/// A short grace window after any operator-driven `set(true)` swallows
/// the bubbling click that originated from the AUTO toggle, so the
/// state doesn't flip-flop in a single click.

const IDLE_MS = 10 * 60 * 1000;
const TOGGLE_GRACE_MS = 750;
const IDLE_POLL_MS = 30_000;

/** data-* attribute the toggle button (and any other "operator
 *  control") sets on itself so click bubbling through the document
 *  listener doesn't treat operator's deliberate toggle as ambient
 *  "interaction" that should disable auto. */
export const AUTO_WATCH_IGNORE_ATTR = "data-auto-watch-ignore";

export interface AutoWatchAPI {
  /** Current mode. */
  on: boolean;
  /** Force a state from operator code. Auto-on/off triggers may
   *  override this later. */
  set: (on: boolean) => void;
  /** Why the mode is what it is right now — surfaced in the tooltip. */
  reason: "operator" | "interaction" | "blur" | "idle";
}

export function useAutoWatch(defaultOn: boolean = false): AutoWatchAPI {
  const [on, setOnState] = useState<boolean>(defaultOn);
  const [reason, setReason] = useState<AutoWatchAPI["reason"]>("operator");
  const lastInteractionAt = useRef<number>(Date.now());
  const onRef = useRef<boolean>(defaultOn);
  const lastOperatorSetAt = useRef<number>(0);

  const applyState = useCallback((next: boolean, why: AutoWatchAPI["reason"]) => {
    onRef.current = next;
    setOnState(next);
    setReason(why);
    if (why === "operator") lastOperatorSetAt.current = Date.now();
  }, []);

  const set = useCallback(
    (next: boolean) => applyState(next, "operator"),
    [applyState],
  );

  useEffect(() => {
    if (typeof window === "undefined") return;

    function withinIgnoredControl(target: EventTarget | null): boolean {
      if (!(target instanceof Element)) return false;
      return target.closest(`[${AUTO_WATCH_IGNORE_ATTR}]`) !== null;
    }

    function onInteraction(e: Event) {
      // Operator toggling AUTO via the button shouldn't count as
      // ambient interaction. The button marks itself with the
      // data-auto-watch-ignore attribute.
      if (withinIgnoredControl(e.target)) return;
      // Short grace period after operator turned AUTO on — the
      // very click that did so may still be bubbling.
      if (Date.now() - lastOperatorSetAt.current < TOGGLE_GRACE_MS) return;
      lastInteractionAt.current = Date.now();
      if (onRef.current) applyState(false, "interaction");
    }
    function onBlur() {
      if (!onRef.current) applyState(true, "blur");
    }
    function onFocus() {
      // Don't auto-toggle on focus return — let idle/interaction
      // decide. Just refresh the idle clock so the operator gets a
      // fresh 10min before idle re-engages auto.
      lastInteractionAt.current = Date.now();
    }

    // Bubble phase, not capture: gives stopPropagation in operator
    // handlers a chance to suppress us cleanly if ever needed, and
    // is sufficient for our purposes.
    document.addEventListener("click", onInteraction);
    document.addEventListener("keydown", onInteraction);
    window.addEventListener("blur", onBlur);
    window.addEventListener("focus", onFocus);

    const idleCheckId = window.setInterval(() => {
      const idleFor = Date.now() - lastInteractionAt.current;
      if (idleFor >= IDLE_MS && !onRef.current) applyState(true, "idle");
    }, IDLE_POLL_MS);

    return () => {
      document.removeEventListener("click", onInteraction);
      document.removeEventListener("keydown", onInteraction);
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("focus", onFocus);
      window.clearInterval(idleCheckId);
    };
  }, [applyState]);

  return { on, set, reason };
}
