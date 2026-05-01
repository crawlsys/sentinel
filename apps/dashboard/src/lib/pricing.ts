// Mirror of crates/sentinel-domain/src/pricing.rs constants for client-side
// use. Kept intentionally tiny — SENTINEL-22 will replace this with a proper
// domain layer.
//
// Source of truth lives in the Rust crate; this file must be kept in sync
// when prices change. See `crates/sentinel-domain/src/pricing.rs`.

export const PRICING_VERSION = "stub";

/** USD per 1M input tokens, by model. Empty for now — fill in SENTINEL-22. */
export const INPUT_PRICE_PER_MTOK: Readonly<Record<string, number>> = {};

/** USD per 1M output tokens, by model. Empty for now — fill in SENTINEL-22. */
export const OUTPUT_PRICE_PER_MTOK: Readonly<Record<string, number>> = {};
