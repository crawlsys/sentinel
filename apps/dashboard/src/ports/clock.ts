// SENTINEL-23 — Clock port.
//
// Indirection over `new Date()` so application logic and tests can swap in
// a fake clock without monkey-patching globals. Single-method by design —
// don't grow this; if you need monotonic ticks or timers, add a separate
// port.

export interface Clock {
  now(): Date;
}
