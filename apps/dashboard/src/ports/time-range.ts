// SENTINEL-23 — TimeRange query type.
//
// Half-open `[start, end)` window used by every read-side port to scope a
// query. Lives in ports/ rather than domain/ because it has no business
// rules — it's a query DTO consumed by adapters.

export interface TimeRange {
  readonly start: Date;
  readonly end: Date;
}
