// SENTINEL-22 — Story point domain types.
//
// Mirrors crates/sentinel-application/src/cost_per_point.rs (SEN-13):
// canonical Linear estimates are the Fibonacci-ish set {1, 2, 3, 5, 8, 16}
// and `bucketEstimate` rounds non-canonical values to the nearest bucket
// with ties rounding UP (so 6 -> 8, 12 -> 16).

export type StoryPoint = number & { readonly __brand: "StoryPoint" };

/** Alias for places where the value semantically represents an estimate. */
export type Estimate = StoryPoint;

/** Canonical Linear estimate buckets. */
export type EstimateBucket = 1 | 2 | 3 | 5 | 8 | 16;

/** All canonical buckets in ascending order. */
export const BUCKETS: readonly EstimateBucket[] = [1, 2, 3, 5, 8, 16];

const VALID_FIB: ReadonlySet<number> = new Set(BUCKETS);

/**
 * Build a validated StoryPoint from a raw number.
 *
 * Throws on:
 *   - non-finite input,
 *   - non-integer input,
 *   - any value not in the canonical Fibonacci-ish set {1, 2, 3, 5, 8, 16}.
 *
 * Use `bucketEstimate` if you need to round an arbitrary value to the
 * nearest canonical bucket.
 */
export function makeStoryPoint(value: number): StoryPoint {
  if (!Number.isFinite(value)) {
    throw new TypeError(`StoryPoint must be finite, got ${value}`);
  }
  if (!Number.isInteger(value)) {
    throw new RangeError(`StoryPoint must be an integer, got ${value}`);
  }
  if (!VALID_FIB.has(value)) {
    throw new RangeError(
      `StoryPoint must be one of ${BUCKETS.join(", ")}, got ${value}`,
    );
  }
  return value as StoryPoint;
}

/**
 * Round an arbitrary numeric estimate to the nearest canonical bucket.
 *
 * Ties round UP — so 4 -> 5 (not 3), 6 -> 8 (not 5), 12 -> 16 (not 8) —
 * matching the Rust `nearest_bucket` in cost_per_point.rs. Above the
 * largest bucket (16) clamps to 16; below 1 clamps to 1.
 */
export function bucketEstimate(value: number): EstimateBucket {
  if (!Number.isFinite(value)) {
    throw new TypeError(`bucketEstimate requires a finite number, got ${value}`);
  }
  if (value <= 1) return 1;
  if (value >= 16) return 16;

  let best: EstimateBucket = BUCKETS[0]!;
  let bestDist = Math.abs(BUCKETS[0]! - value);
  for (const b of BUCKETS.slice(1)) {
    const d = Math.abs(b - value);
    // Strictly less -> take it; equal (tie) -> take the larger.
    // Iterating ascending means a tie with the prior bucket already updates
    // `best` to the larger one here.
    if (d < bestDist || Math.abs(d - bestDist) < Number.EPSILON) {
      best = b;
      bestDist = d;
    }
  }
  return best;
}
