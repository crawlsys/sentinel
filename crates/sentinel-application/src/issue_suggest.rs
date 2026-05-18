//! Auto-suggest at issue creation — SEN-4.
//!
//! Given a freshly-created Linear issue's title, returns suggested
//! `estimate`, `priority`, and `labels` based on similarity to the
//! issue's historical neighbours. Pure data layer; the Hookdeck
//! `Issue.create` webhook handler that calls this and posts the
//! suggestion back as a Linear comment is a follow-up.
//!
//! ## Algorithm
//!
//! 1. Tokenise the new title (lowercase, split on non-alphanumeric,
//!    drop tokens < 3 chars and a small stop-word list).
//! 2. For every historical issue, compute Jaccard similarity over the
//!    token sets.
//! 3. Take the top-K most-similar historical issues; aggregate their
//!    fields:
//!    - `estimate` → median of non-null estimates.
//!    - `priority` → mode of non-null priorities (most common wins;
//!      ties broken by lower priority number = more urgent).
//!    - `labels` → labels appearing in ≥ 50% of the K neighbours.
//! 4. Report each suggestion alongside a confidence ([`Confidence`])
//!    based on the count of usable neighbours.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::cycle_time_prediction::Confidence;

/// Default K (number of neighbours used for the aggregate).
pub const DEFAULT_K: usize = 8;

/// Minimum Jaccard similarity for a neighbour to count.
pub const MIN_SIMILARITY: f64 = 0.2;

/// Tokens shorter than this are dropped.
pub const MIN_TOKEN_LEN: usize = 3;

/// One historical issue used as a similarity neighbour. Producers pull
/// these from the Linear `getCompletedTickets` API or any equivalent
/// archive; this module treats them as opaque past examples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalIssue {
    pub identifier: String,
    pub title: String,
    pub team: Option<String>,
    pub priority: Option<u8>,
    pub estimate: Option<u32>,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// One auto-suggest result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSuggest {
    pub title: String,
    pub team: Option<String>,
    pub suggested_estimate: Option<u32>,
    pub suggested_priority: Option<u8>,
    pub suggested_labels: Vec<String>,
    pub confidence: Confidence,
    pub neighbours_considered: usize,
    pub neighbours: Vec<NeighbourMatch>,
}

/// One scored historical neighbour surfaced in the suggestion payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighbourMatch {
    pub identifier: String,
    pub title: String,
    pub similarity: f64,
}

/// Tiny stop-word list to drop title noise. Kept inline so SEN-4 doesn't
/// pull a full NLP dep just for stop-words.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "into", "that", "this", "fix",
    "feat", "add", "use", "via", "are", "but", "not", "you", "your",
    "our", "all", "out", "can", "has", "had", "was", "will", "should",
];

/// Tokenise a title into a sorted unique set. Lowercase, split on
/// non-alphanumerics, drop short tokens and the stop-word list.
#[must_use]
pub fn tokenise(title: &str) -> BTreeSet<String> {
    title
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|t| t.len() >= MIN_TOKEN_LEN && !STOP_WORDS.contains(&t.as_str()))
        .collect()
}

/// Jaccard similarity between two token sets: `|A ∩ B| / |A ∪ B|`.
/// Returns `0.0` when both sets are empty.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Produce an [`AutoSuggest`] for a candidate title given a corpus of
/// historical issues.
#[must_use]
pub fn suggest_for_title(
    title: &str,
    team: Option<&str>,
    history: &[HistoricalIssue],
    k: usize,
) -> AutoSuggest {
    let target = tokenise(title);

    // Score every historical issue; prefer same-team rows by adding a
    // small constant boost (0.1) so similarity-tied issues from the
    // current team win.
    let mut scored: Vec<(f64, &HistoricalIssue)> = history
        .iter()
        .filter_map(|h| {
            let sim = jaccard(&target, &tokenise(&h.title));
            if sim < MIN_SIMILARITY {
                return None;
            }
            let bonus = match (team, h.team.as_deref()) {
                (Some(t), Some(ht)) if t == ht => 0.1,
                _ => 0.0,
            };
            Some((sim + bonus, h))
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let neighbours_used: Vec<_> = scored.into_iter().take(k).collect();

    let estimates: Vec<u32> = neighbours_used
        .iter()
        .filter_map(|(_, h)| h.estimate)
        .collect();
    let suggested_estimate = median_u32(&estimates);

    let priorities: Vec<u8> = neighbours_used
        .iter()
        .filter_map(|(_, h)| h.priority)
        .collect();
    let suggested_priority = mode_u8(&priorities);

    // Labels: present in >= ceil(50%) of usable neighbours.
    let n = neighbours_used.len();
    let half = n.div_ceil(2);
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    for (_, h) in &neighbours_used {
        let mut seen = BTreeSet::new();
        for l in &h.labels {
            if seen.insert(l.clone()) {
                *label_counts.entry(l.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut suggested_labels: Vec<String> = label_counts
        .into_iter()
        .filter(|(_, c)| n > 0 && *c >= half)
        .map(|(l, _)| l)
        .collect();
    suggested_labels.sort();

    AutoSuggest {
        title: title.to_string(),
        team: team.map(str::to_string),
        suggested_estimate,
        suggested_priority,
        suggested_labels,
        confidence: Confidence::from_sample_count(n),
        neighbours_considered: n,
        neighbours: neighbours_used
            .iter()
            .map(|(sim, h)| NeighbourMatch {
                identifier: h.identifier.clone(),
                title: h.title.clone(),
                similarity: *sim,
            })
            .collect(),
    }
}

fn median_u32(values: &[u32]) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n.is_multiple_of(2) {
        Some((v[n / 2 - 1] + v[n / 2]) / 2)
    } else {
        Some(v[n / 2])
    }
}

/// Mode (most-frequent value) over a `u8` slice. Ties broken by smaller
/// value (= more urgent priority). Returns `None` on empty input.
fn mode_u8(values: &[u8]) -> Option<u8> {
    if values.is_empty() {
        return None;
    }
    let mut counts: BTreeMap<u8, usize> = BTreeMap::new();
    for v in values {
        *counts.entry(*v).or_insert(0) += 1;
    }
    let max = counts.values().copied().max().unwrap_or(0);
    counts
        .into_iter()
        .filter(|(_, c)| *c == max)
        .map(|(v, _)| v)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str, title: &str, team: Option<&str>, est: Option<u32>, pri: Option<u8>, labels: &[&str]) -> HistoricalIssue {
        HistoricalIssue {
            identifier: id.to_string(),
            title: title.to_string(),
            team: team.map(str::to_string),
            priority: pri,
            estimate: est,
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn tokenise_lowercases_and_drops_short_and_stop() {
        let t = tokenise("Fix the calendar deep-link for FPCRM");
        // "fix", "the", "for" → stop words; "FPCRM" → "fpcrm" (5 chars, kept).
        assert!(t.contains("calendar"));
        assert!(t.contains("deep"));
        assert!(t.contains("link"));
        assert!(t.contains("fpcrm"));
        assert!(!t.contains("fix"));
        assert!(!t.contains("the"));
        assert!(!t.contains("for"));
    }

    #[test]
    fn jaccard_overlapping_sets() {
        let a: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| (*s).to_string()).collect();
        let b: BTreeSet<String> = ["b", "c", "d"].iter().map(|s| (*s).to_string()).collect();
        // Intersection {b, c} = 2; Union {a, b, c, d} = 4; 2/4 = 0.5.
        assert!((jaccard(&a, &b) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_disjoint_is_zero() {
        let a: BTreeSet<String> = ["a"].iter().map(|s| (*s).to_string()).collect();
        let b: BTreeSet<String> = ["b"].iter().map(|s| (*s).to_string()).collect();
        assert!((jaccard(&a, &b) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn suggest_empty_history_returns_no_suggestions() {
        let s = suggest_for_title("fix login bug", Some("FPCRM"), &[], DEFAULT_K);
        assert_eq!(s.neighbours_considered, 0);
        assert_eq!(s.suggested_estimate, None);
        assert_eq!(s.suggested_priority, None);
        assert!(s.suggested_labels.is_empty());
        assert_eq!(s.confidence, Confidence::Low);
    }

    #[test]
    fn suggest_picks_top_k_neighbours_by_similarity() {
        let history = vec![
            issue("FPCRM-1", "fix login authentication bug", Some("FPCRM"), Some(3), Some(2), &["bug", "auth"]),
            issue("FPCRM-2", "fix login validation error", Some("FPCRM"), Some(3), Some(2), &["bug", "auth"]),
            issue("FPCRM-3", "fix login redirect after sso", Some("FPCRM"), Some(5), Some(2), &["bug"]),
            issue("FPCRM-99", "completely unrelated payment processing fix", Some("FPCRM"), Some(8), Some(3), &["payments"]),
        ];
        let s = suggest_for_title("fix login session bug", Some("FPCRM"), &history, DEFAULT_K);
        // Three login issues match; the payments one is below MIN_SIMILARITY → dropped.
        assert_eq!(s.neighbours_considered, 3);
        // Median of [3, 3, 5] = 3.
        assert_eq!(s.suggested_estimate, Some(3));
        // Mode of [2, 2, 2] = 2.
        assert_eq!(s.suggested_priority, Some(2));
        // "bug" appears 3/3 → kept; "auth" 2/3 = 67% → kept (>= ceil(3/2)=2).
        assert!(s.suggested_labels.contains(&"bug".to_string()));
        assert!(s.suggested_labels.contains(&"auth".to_string()));
        // "payments" not on any neighbour → dropped.
        assert!(!s.suggested_labels.contains(&"payments".to_string()));
    }

    #[test]
    fn suggest_drops_neighbours_below_min_similarity() {
        let history = vec![
            issue("FPCRM-1", "fix login bug", Some("FPCRM"), Some(3), Some(2), &["bug"]),
            issue("FPCRM-2", "completely different topic about payment processing", Some("FPCRM"), Some(13), Some(1), &["payments"]),
        ];
        let s = suggest_for_title("fix login session", Some("FPCRM"), &history, DEFAULT_K);
        assert_eq!(s.neighbours_considered, 1);
        assert_eq!(s.suggested_estimate, Some(3));
    }

    #[test]
    fn suggest_team_boost_breaks_similarity_ties() {
        // Two equally-similar historical issues, one on the asking team
        // (FPCRM) and one not (OTHER). The team boost should rank FPCRM
        // first; with k=1 only the FPCRM neighbour drives the suggestion.
        let history = vec![
            issue("OTHER-1", "fix calendar invite bug", Some("OTHER"), Some(8), Some(3), &["calendar"]),
            issue("FPCRM-1", "fix calendar invite bug", Some("FPCRM"), Some(2), Some(1), &["calendar"]),
        ];
        let s = suggest_for_title("fix calendar invite bug", Some("FPCRM"), &history, 1);
        assert_eq!(s.suggested_estimate, Some(2));
        assert_eq!(s.suggested_priority, Some(1));
    }

    #[test]
    fn suggest_label_threshold_is_half_round_up() {
        let history = vec![
            issue("X-1", "fix login bug", Some("X"), Some(3), Some(2), &["a", "b"]),
            issue("X-2", "fix login bug session", Some("X"), Some(3), Some(2), &["a"]),
            issue("X-3", "fix login bug auth", Some("X"), Some(3), Some(2), &["a"]),
        ];
        let s = suggest_for_title("fix login bug", Some("X"), &history, DEFAULT_K);
        // 3 neighbours; half = ceil(3/2) = 2. "a" appears 3 times → kept;
        // "b" appears 1 time → dropped.
        assert!(s.suggested_labels.contains(&"a".to_string()));
        assert!(!s.suggested_labels.contains(&"b".to_string()));
    }

    #[test]
    fn suggest_confidence_reflects_neighbour_count() {
        // Just one neighbour above threshold → Low.
        let history = vec![
            issue("X-1", "fix login bug", Some("X"), Some(3), Some(2), &[]),
        ];
        let s = suggest_for_title("fix login bug", Some("X"), &history, DEFAULT_K);
        assert_eq!(s.confidence, Confidence::Low);
    }

    #[test]
    fn median_u32_handles_odd_and_even() {
        assert_eq!(median_u32(&[1, 2, 3]), Some(2));
        assert_eq!(median_u32(&[1, 2, 3, 4]), Some(2)); // (2+3)/2 = 2 (int div).
        assert_eq!(median_u32(&[]), None);
    }

    #[test]
    fn mode_u8_breaks_ties_by_lower_value() {
        // Counts: 1→2, 2→2 (tie). Lower wins → 1.
        assert_eq!(mode_u8(&[1, 1, 2, 2]), Some(1));
        assert_eq!(mode_u8(&[3, 3, 3, 1]), Some(3));
        assert_eq!(mode_u8(&[]), None);
    }
}
