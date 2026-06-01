"use client";

import type { GraphResponse, Node } from "../types/api";

/** A session is "stuck" if it's been waiting on a user reply for
 *  more than this many seconds. The bridge's freshness gate expires
 *  awaiting_user after 1h; anything past 15m here is a real ask
 *  that hasn't been picked up. */
export const STUCK_THRESHOLD_SECS = 900;

/** How many stuck sessions trigger the "you should look at this"
 *  browser notification. */
export const STUCK_ALERT_COUNT = 3;

/** Don't re-alert within this many ms of the prior alert. */
export const STUCK_ALERT_SUPPRESS_MS = 5 * 60 * 1000;

export function isStuck(node: Node): boolean {
  if (node.type !== "SentinelSession") return false;
  if (node.session_status !== "awaiting_user") return false;
  const age = node.last_activity_age_s ?? 0;
  return age > STUCK_THRESHOLD_SECS;
}

export function stuckSessions(graph: GraphResponse | null): Node[] {
  if (!graph) return [];
  return graph.nodes.filter(isStuck);
}

interface AlertState {
  lastFiredAt: number;
  lastSeenCount: number;
}

/** Module-singleton alert state. Survives re-renders so we don't
 *  re-fire on every SSE tick. */
const state: AlertState = {
  lastFiredAt: 0,
  lastSeenCount: 0,
};

/** Trigger a browser Notification once when the stuck count crosses
 *  STUCK_ALERT_COUNT for the first time after a fall-below event.
 *  Suppressed if a prior alert fired within STUCK_ALERT_SUPPRESS_MS.
 *  Caller should invoke this on every graph update. */
export function maybeFireStuckAlert(stuckCount: number, stuck: Node[]): void {
  if (typeof window === "undefined" || typeof Notification === "undefined") {
    state.lastSeenCount = stuckCount;
    return;
  }
  const wasUnderThreshold = state.lastSeenCount < STUCK_ALERT_COUNT;
  const nowOverThreshold = stuckCount >= STUCK_ALERT_COUNT;
  state.lastSeenCount = stuckCount;
  if (!(wasUnderThreshold && nowOverThreshold)) return;

  const now = Date.now();
  if (now - state.lastFiredAt < STUCK_ALERT_SUPPRESS_MS) return;
  state.lastFiredAt = now;

  // Only fire when permission is ALREADY granted. We never call
  // Notification.requestPermission() from here — this path is driven
  // by SSE data arrival, not a user gesture, and modern browsers
  // reject (and may flag as abusive) non-gesture permission prompts.
  // The prompt is requested from a real click via
  // requestStuckNotificationPermission() instead.
  if (Notification.permission === "granted") {
    fireStuckNotification(stuckCount, stuck);
  }
}

function fireStuckNotification(stuckCount: number, stuck: Node[]): void {
  try {
    const n = new Notification(`${stuckCount} sentinel sessions awaiting you`, {
      body: stuck
        .slice(0, 3)
        .map((s) => {
          const sid = (s.data?.session_id as string | undefined) ?? s.id;
          return `${sid.slice(0, 8)}: ${s.awaiting_question?.slice(0, 90) ?? "(no question)"}`;
        })
        .join("\n"),
      icon: "/favicon.ico",
      tag: "sentinel-stuck",
    });
    n.onclick = () => {
      window.focus();
      n.close();
    };
  } catch {
    /* notification denied or unsupported — silent */
  }
}

/** Request browser Notification permission. MUST be called from a real
 *  user gesture (e.g. a click handler) — never from a render/effect —
 *  or browsers will reject the prompt. Safe to call repeatedly; resolves
 *  to the current permission state. Returns "unsupported" off the main
 *  thread or where Notification is unavailable. */
export async function requestStuckNotificationPermission(): Promise<
  NotificationPermission | "unsupported"
> {
  if (typeof window === "undefined" || typeof Notification === "undefined") {
    return "unsupported";
  }
  if (Notification.permission !== "default") return Notification.permission;
  try {
    return await Notification.requestPermission();
  } catch {
    return Notification.permission;
  }
}

/** Reset module state. Test-only. */
export function _resetStuckAlertState() {
  state.lastFiredAt = 0;
  state.lastSeenCount = 0;
}
