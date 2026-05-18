// SENTINEL-24 — System clock adapter.
//
// Trivial Clock implementation backed by `new Date()`. Lives here so the
// composition root can hand the app layer a Clock instance without the
// application/ folder importing wall-clock state directly.

import type { Clock } from "../ports";

export class SystemClock implements Clock {
  now(): Date {
    return new Date();
  }
}
