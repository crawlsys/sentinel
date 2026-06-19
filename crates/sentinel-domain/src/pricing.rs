//! Anthropic per-model token pricing (USD per million tokens).
//!
//! Pure domain logic — no IO, no clock. Used by the
//! `tokens-per-ticket` aggregator (SEN-7) to convert raw `usage`
//! blocks from session JSONL into a USD cost estimate.
//!
//! TODO(follow-up): move these constants to a TOML lookup so that
//! pricing changes do not require a sentinel rebuild. For now they
//! are hardcoded — Anthropic publishes rates infrequently enough
//! that this is acceptable for a Phase-1 metrics aggregator.

use serde::{Deserialize, Serialize};

/// Token counts for a single `usage` block.
///
/// Mirrors the shape Anthropic returns under `message.usage` in
/// session JSONL files. `cache_creation_5m` / `cache_creation_1h`
/// are populated from the nested `cache_creation` object when the
/// session JSONL exposes it; if only the flat
/// `cache_creation_input_tokens` field is present the caller folds
/// it into `cache_creation_5m` (cheaper rate, matches default TTL).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation_5m: u64,
    pub cache_creation_1h: u64,
}

impl TokenUsage {
    #[must_use]
    pub const fn add(self, other: Self) -> Self {
        Self {
            input: self.input + other.input,
            output: self.output + other.output,
            cache_read: self.cache_read + other.cache_read,
            cache_creation_5m: self.cache_creation_5m + other.cache_creation_5m,
            cache_creation_1h: self.cache_creation_1h + other.cache_creation_1h,
        }
    }
}

/// Coarse pricing tier — Opus / Sonnet / Haiku. Unknown models are pricing
/// errors; callers must surface unpriced usage instead of inventing a tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingTier {
    Opus,
    Sonnet,
    Haiku,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingError {
    model: String,
}

impl PricingError {
    #[must_use]
    pub fn unknown_model(model: &str) -> Self {
        Self {
            model: model.to_string(),
        }
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl std::fmt::Display for PricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.model.trim().is_empty() {
            f.write_str("cannot price token usage without a concrete model id")
        } else {
            write!(f, "unknown model id for token pricing: {}", self.model)
        }
    }
}

impl std::error::Error for PricingError {}

/// Per-million-token USD rates for one tier.
#[derive(Debug, Clone, Copy)]
struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_creation_5m: f64,
    cache_creation_1h: f64,
}

const OPUS_RATES: Rates = Rates {
    input: 15.0,
    output: 75.0,
    cache_read: 1.50,
    cache_creation_5m: 3.75,
    cache_creation_1h: 18.75,
};

const SONNET_RATES: Rates = Rates {
    input: 3.0,
    output: 15.0,
    cache_read: 0.30,
    cache_creation_5m: 0.75,
    cache_creation_1h: 3.75,
};

const HAIKU_RATES: Rates = Rates {
    input: 0.80,
    output: 4.0,
    cache_read: 0.08,
    cache_creation_5m: 0.20,
    cache_creation_1h: 1.0,
};

const fn rates_for(tier: PricingTier) -> Rates {
    match tier {
        PricingTier::Opus => OPUS_RATES,
        PricingTier::Sonnet => SONNET_RATES,
        PricingTier::Haiku => HAIKU_RATES,
    }
}

/// Classify an Anthropic model id (`claude-opus-4-6`,
/// `claude-sonnet-4-5`, `claude-haiku-4-5`, …) into a pricing tier.
///
/// Unknown ids are rejected. Matching is case-insensitive and tolerates
/// `[1m]`-style suffixes.
pub fn tier_for_model(model: &str) -> Result<PricingTier, PricingError> {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") {
        Ok(PricingTier::Haiku)
    } else if m.contains("sonnet") {
        Ok(PricingTier::Sonnet)
    } else if m.contains("opus") {
        Ok(PricingTier::Opus)
    } else {
        Err(PricingError::unknown_model(model))
    }
}

/// Compute USD cost for a `usage` block under the given model id.
pub fn cost_for(usage: TokenUsage, model: &str) -> Result<f64, PricingError> {
    let r = rates_for(tier_for_model(model)?);
    let per_mtok = 1_000_000.0_f64;
    #[allow(clippy::cast_precision_loss)]
    let input = (usage.input as f64) * r.input / per_mtok;
    #[allow(clippy::cast_precision_loss)]
    let output = (usage.output as f64) * r.output / per_mtok;
    #[allow(clippy::cast_precision_loss)]
    let cache_read = (usage.cache_read as f64) * r.cache_read / per_mtok;
    #[allow(clippy::cast_precision_loss)]
    let short_cache_create = (usage.cache_creation_5m as f64) * r.cache_creation_5m / per_mtok;
    #[allow(clippy::cast_precision_loss)]
    let long_cache_create = (usage.cache_creation_1h as f64) * r.cache_creation_1h / per_mtok;
    Ok(input + output + cache_read + short_cache_create + long_cache_create)
}

/// Short, operator-friendly model label (e.g. `opus-4-7`,
/// `sonnet-4-5`) extracted from a full Anthropic model id. Falls
/// back to the original string if no `claude-` prefix is found.
#[must_use]
pub fn short_model_label(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    // Strip optional "claude-" prefix.
    let trimmed = m.strip_prefix("claude-").unwrap_or(&m);
    // Drop suffixes like "[1m]" used by the harness for context tier.
    let trimmed = trimmed.split('[').next().unwrap_or(trimmed);
    trimmed.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_models() {
        assert_eq!(
            tier_for_model("claude-opus-4-6").unwrap(),
            PricingTier::Opus
        );
        assert_eq!(
            tier_for_model("claude-opus-4-7").unwrap(),
            PricingTier::Opus
        );
        assert_eq!(
            tier_for_model("claude-sonnet-4-5").unwrap(),
            PricingTier::Sonnet
        );
        assert_eq!(
            tier_for_model("claude-sonnet-4-6").unwrap(),
            PricingTier::Sonnet
        );
        assert_eq!(
            tier_for_model("claude-haiku-4-5").unwrap(),
            PricingTier::Haiku
        );
    }

    #[test]
    fn unknown_models_are_pricing_errors() {
        assert!(tier_for_model("totally-made-up-model").is_err());
        assert!(tier_for_model("").is_err());
    }

    #[test]
    fn cost_matches_published_opus_rates() {
        // 1 Mtok input = $15 on opus
        let usage = TokenUsage {
            input: 1_000_000,
            ..Default::default()
        };
        assert!((cost_for(usage, "claude-opus-4-7").unwrap() - 15.0).abs() < 0.001);

        // 1 Mtok output = $75 on opus
        let usage = TokenUsage {
            output: 1_000_000,
            ..Default::default()
        };
        assert!((cost_for(usage, "claude-opus-4-7").unwrap() - 75.0).abs() < 0.001);

        // Cache read on opus is $1.50/Mtok
        let usage = TokenUsage {
            cache_read: 1_000_000,
            ..Default::default()
        };
        assert!((cost_for(usage, "claude-opus-4-7").unwrap() - 1.50).abs() < 0.001);

        // Mixed example: 124_800 input + 18_420 output + 3_920_000 cache_read
        let usage = TokenUsage {
            input: 124_800,
            output: 18_420,
            cache_read: 3_920_000,
            ..Default::default()
        };
        // 124800/1e6*15 + 18420/1e6*75 + 3920000/1e6*1.5
        // = 1.872 + 1.3815 + 5.88 = 9.1335
        assert!((cost_for(usage, "claude-opus-4-7").unwrap() - 9.1335).abs() < 0.01);
    }

    #[test]
    fn sonnet_rates_are_applied_for_sonnet_models() {
        let usage = TokenUsage {
            input: 1_000_000,
            ..Default::default()
        };
        assert!((cost_for(usage, "claude-sonnet-4-5").unwrap() - 3.0).abs() < 0.001);
    }

    #[test]
    fn short_label_strips_claude_prefix_and_suffixes() {
        assert_eq!(short_model_label("claude-opus-4-7"), "opus-4-7");
        assert_eq!(short_model_label("claude-sonnet-4-5"), "sonnet-4-5");
        assert_eq!(short_model_label("claude-opus-4-7[1m]"), "opus-4-7");
        assert_eq!(short_model_label("custom-model"), "custom-model");
    }
}
