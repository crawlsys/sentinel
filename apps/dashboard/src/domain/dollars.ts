// SENTINEL-22 — Dollars + token cost domain types.
//
// Mirrors crates/sentinel-domain/src/pricing.rs (SEN-7) so server- and
// client-side cost calculations stay in lockstep. Per-million-token rates
// are hardcoded — Anthropic publishes infrequently enough that rebuilds
// are acceptable, and a TOML lookup is a future enhancement on both sides.

export type Dollars = number & { readonly __brand: "Dollars" };
export type Tokens = number & { readonly __brand: "Tokens" };

/** Token counts for a single Anthropic `usage` block. */
export interface TokenUsage {
  readonly input: number;
  readonly output: number;
  readonly cache_read: number;
  readonly cache_creation_5m: number;
  readonly cache_creation_1h: number;
}

/** Coarse pricing tier — Opus / Sonnet / Haiku. */
export type PricingTier = "opus" | "sonnet" | "haiku";

/** Per-million-token USD rates for one tier. */
export interface Rates {
  readonly input: number;
  readonly output: number;
  readonly cache_read: number;
  readonly cache_creation_5m: number;
  readonly cache_creation_1h: number;
}

export const OPUS_RATES: Rates = Object.freeze({
  input: 15.0,
  output: 75.0,
  cache_read: 1.5,
  cache_creation_5m: 3.75,
  cache_creation_1h: 18.75,
});

export const SONNET_RATES: Rates = Object.freeze({
  input: 3.0,
  output: 15.0,
  cache_read: 0.3,
  cache_creation_5m: 0.75,
  cache_creation_1h: 3.75,
});

export const HAIKU_RATES: Rates = Object.freeze({
  input: 0.8,
  output: 4.0,
  cache_read: 0.08,
  cache_creation_5m: 0.2,
  cache_creation_1h: 1.0,
});

/** Lookup by short label (e.g. `opus-4-7`). */
export const RATES_BY_MODEL: Readonly<Record<string, Rates>> = Object.freeze({
  "opus-4-6": OPUS_RATES,
  "opus-4-7": OPUS_RATES,
  "sonnet-4-5": SONNET_RATES,
  "sonnet-4-6": SONNET_RATES,
  "haiku-4-5": HAIKU_RATES,
});

const PER_MTOK = 1_000_000;

/**
 * Classify an Anthropic model id into a pricing tier. Unknown ids fall
 * back to Opus rates so cost estimates never under-report. Matching is
 * case-insensitive and tolerates `[1m]`-style suffixes.
 */
export function tierForModel(model: string): PricingTier {
  const m = model.toLowerCase();
  if (m.includes("haiku")) return "haiku";
  if (m.includes("sonnet")) return "sonnet";
  return "opus";
}

function ratesFor(tier: PricingTier): Rates {
  switch (tier) {
    case "haiku":
      return HAIKU_RATES;
    case "sonnet":
      return SONNET_RATES;
    case "opus":
      return OPUS_RATES;
  }
}

/**
 * Construct a Dollars value from a raw number. Throws on non-finite input.
 * Negative values are allowed (e.g. credits).
 */
export function makeDollars(value: number): Dollars {
  if (!Number.isFinite(value)) {
    throw new TypeError(`Dollars must be finite, got ${value}`);
  }
  return value as Dollars;
}

/** Construct a Tokens value. Throws on negative or non-finite input. */
export function makeTokens(value: number): Tokens {
  if (!Number.isFinite(value)) {
    throw new TypeError(`Tokens must be finite, got ${value}`);
  }
  if (value < 0) {
    throw new RangeError(`Tokens must be >= 0, got ${value}`);
  }
  return value as Tokens;
}

/**
 * Compute USD cost for a `usage` block under the given model id.
 *
 * Mirrors `cost_for` in pricing.rs exactly — same per-Mtok rates, same
 * five-component sum, same fallback behaviour for unknown models.
 */
export function cost(usage: TokenUsage, model: string): Dollars {
  const r = ratesFor(tierForModel(model));
  const total =
    (usage.input * r.input) / PER_MTOK +
    (usage.output * r.output) / PER_MTOK +
    (usage.cache_read * r.cache_read) / PER_MTOK +
    (usage.cache_creation_5m * r.cache_creation_5m) / PER_MTOK +
    (usage.cache_creation_1h * r.cache_creation_1h) / PER_MTOK;
  return total as Dollars;
}

/**
 * Short, dashboard-friendly model label (`opus-4-7`, `sonnet-4-5`, ...).
 * Strips a leading `claude-` and any `[1m]`-style suffix. Falls back to
 * the lowercased original if no `claude-` prefix is found.
 */
export function shortModelLabel(model: string): string {
  const m = model.toLowerCase();
  const noPrefix = m.startsWith("claude-") ? m.slice("claude-".length) : m;
  const noSuffix = noPrefix.split("[")[0] ?? noPrefix;
  return noSuffix.replace(/-+$/, "");
}
