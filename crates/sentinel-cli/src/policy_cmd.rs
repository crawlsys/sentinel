//! `sentinel policy suggest` — turn plain-English policy statements into
//! ready-to-paste sentinel config TOML (sentinel #59 / M7.10 — AEGIS pattern).
//!
//! Today's surface: parse a one-line policy statement like
//! `"linear/qa-handoff/3.5.5 requires browserbase verified"` and emit a
//! `[[step_verifiers]]` TOML fragment a human can paste into the sentinel
//! handler bootstrap (or into a future declarative config file).
//!
//! Out of scope for v1:
//! - Multi-line policies, ANY/ALL combinators, time-bound rules.
//! - LLM-assisted interpretation of fuzzier prompts (e.g. "make sure all
//!   QA-handoff steps need Browserbase"). The CLI is the deterministic
//!   floor — a future skill can call an LLM and feed the result through
//!   this same parser.
//! - Inverse direction (TOML → English) — useful for audits, deferred.
//!
//! ## Grammar (deliberately small)
//!
//! ```text
//! policy   := target SP "requires" SP adapter SP mode
//! target   := SKILL "/" PHASE "/" STEP
//! adapter  := IDENT                       # e.g. "browserbase", "filesystem"
//! mode     := "verified" | "provenance"   # defaults to "verified" if omitted
//! ```
//!
//! Identifiers are `[a-zA-Z0-9_.-]+`. Phase ids commonly contain dashes
//! (`qa-handoff`); step ids commonly contain dots (`3.5.5`). The parser
//! is forgiving on case for the mode keyword but strict on order so the
//! error surface stays small.
//!
//! ## Why deterministic
//!
//! The AEGIS pattern in the task name refers to turning natural-language
//! safety rules into machine-checkable policy. The risky version (and
//! deferred work) involves an LLM. The non-risky version — what ships
//! today — is a tiny rule-based parser. It can't hallucinate, it can't
//! pull in surprising adapters, and the failure mode is "your input
//! doesn't match the grammar, here's where it broke" rather than
//! "we generated something plausible-looking but wrong."

use anyhow::{anyhow, bail, Context, Result};

use sentinel_domain::step_verifier::StepVerifierRequirement;

/// A parsed policy statement. Holds the typed structure before we
/// serialize to whatever output format the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySuggestion {
    pub requirement: StepVerifierRequirement,
}

impl PolicySuggestion {
    /// Parse a single policy statement.
    ///
    /// Accepts forms:
    /// - `"<skill>/<phase>/<step> requires <adapter>"`
    ///   → verified-only (the strict production case)
    /// - `"<skill>/<phase>/<step> requires <adapter> verified"`
    ///   → same as above, explicit
    /// - `"<skill>/<phase>/<step> requires <adapter> provenance"`
    ///   → provenance-only (audit, unverified receipts also count)
    ///
    /// # Errors
    /// Returns Err with a message naming where the parse went wrong.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty policy statement");
        }

        // Token 1: the target (skill/phase/step).
        let mut tokens = input.split_whitespace();
        let target = tokens
            .next()
            .ok_or_else(|| anyhow!("missing target — expected skill/phase/step"))?;
        let parts: Vec<&str> = target.split('/').collect();
        if parts.len() != 3 {
            bail!(
                "target must be skill/phase/step (got {} parts): {target}",
                parts.len()
            );
        }
        let (skill, phase_id, step_id) = (parts[0], parts[1], parts[2]);
        if skill.is_empty() || phase_id.is_empty() || step_id.is_empty() {
            bail!("target parts cannot be empty: {target}");
        }
        validate_ident(skill, "skill")?;
        validate_ident(phase_id, "phase")?;
        validate_ident(step_id, "step")?;

        // Token 2: "requires" keyword.
        let kw = tokens
            .next()
            .ok_or_else(|| anyhow!("missing 'requires' keyword after target"))?;
        if !kw.eq_ignore_ascii_case("requires") {
            bail!("expected 'requires' after target, got '{kw}'");
        }

        // Token 3: adapter name.
        let adapter = tokens
            .next()
            .ok_or_else(|| anyhow!("missing adapter name after 'requires'"))?;
        validate_ident(adapter, "adapter")?;

        // Token 4 (optional): mode keyword.
        let mode = match tokens.next() {
            None => "verified",
            Some(m) => m,
        };

        // Reject any trailing garbage.
        if tokens.next().is_some() {
            bail!(
                "unexpected trailing tokens after mode '{mode}' — \
                 grammar accepts at most 4 tokens"
            );
        }

        let requirement = match mode.to_ascii_lowercase().as_str() {
            "verified" => StepVerifierRequirement::new(skill, phase_id, step_id, adapter),
            "provenance" => {
                StepVerifierRequirement::provenance_only(skill, phase_id, step_id, adapter)
            }
            other => bail!(
                "unknown mode '{other}' — accepted: 'verified' (default) or 'provenance'"
            ),
        };
        Ok(PolicySuggestion { requirement })
    }

    /// Render this suggestion as a TOML fragment ready to paste into
    /// a sentinel config (or into the handler bootstrap as a
    /// reviewable comment block).
    ///
    /// Output is opinionated: each fragment includes a header comment
    /// citing the source policy so future readers can trace it back.
    /// The fragment is array-of-tables shaped (`[[step_verifiers]]`)
    /// because that's how Vec<StepVerifierRequirement> serializes
    /// into the standard config schema.
    #[must_use]
    pub fn to_toml_fragment(&self, source_policy: &str) -> String {
        let r = &self.requirement;
        let mode_comment = if r.verified_only {
            "verified-only (default — strict production mode)"
        } else {
            "provenance-only (audit mode — unverified receipts also count)"
        };
        format!(
            "# Generated by `sentinel policy suggest`\n\
             # Source policy: {source_policy}\n\
             # Mode: {mode_comment}\n\
             [[step_verifiers]]\n\
             skill         = {skill:?}\n\
             phase_id      = {phase_id:?}\n\
             step_id       = {step_id:?}\n\
             adapter_name  = {adapter:?}\n\
             verified_only = {verified}\n",
            skill = r.skill,
            phase_id = r.phase_id,
            step_id = r.step_id,
            adapter = r.adapter_name,
            verified = r.verified_only,
        )
    }
}

fn validate_ident(s: &str, role: &str) -> Result<()> {
    if s.is_empty() {
        bail!("{role} identifier is empty");
    }
    let ok = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        bail!(
            "{role} identifier '{s}' contains illegal characters \
             (allowed: a-zA-Z0-9, underscore, dash, dot)"
        );
    }
    Ok(())
}

/// `sentinel policy suggest <policy>` — parse one policy and emit a
/// TOML fragment to stdout. Exit 1 with a clear error if the policy
/// doesn't parse.
pub fn run_suggest(policy: &str) -> Result<()> {
    let suggestion = PolicySuggestion::parse(policy)
        .with_context(|| format!("could not parse policy: {policy:?}"))?;
    let fragment = suggestion.to_toml_fragment(policy);
    println!("{fragment}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verified_default() {
        let s = PolicySuggestion::parse("linear/qa-handoff/3.5.5 requires browserbase").unwrap();
        assert_eq!(s.requirement.skill, "linear");
        assert_eq!(s.requirement.phase_id, "qa-handoff");
        assert_eq!(s.requirement.step_id, "3.5.5");
        assert_eq!(s.requirement.adapter_name, "browserbase");
        assert!(s.requirement.verified_only);
    }

    #[test]
    fn parses_explicit_verified() {
        let s = PolicySuggestion::parse(
            "linear/qa-handoff/3.5.5 requires browserbase verified",
        )
        .unwrap();
        assert!(s.requirement.verified_only);
    }

    #[test]
    fn parses_provenance_mode() {
        let s = PolicySuggestion::parse(
            "linear/qa-handoff/3.5.5 requires browserbase provenance",
        )
        .unwrap();
        assert!(!s.requirement.verified_only);
    }

    #[test]
    fn mode_is_case_insensitive() {
        for mode in ["verified", "VERIFIED", "Verified", "provenance", "PROVENANCE"] {
            let input = format!("linear/qa-handoff/3.5.5 requires browserbase {mode}");
            PolicySuggestion::parse(&input).unwrap_or_else(|e| {
                panic!("case-insensitive parse failed for '{mode}': {e}")
            });
        }
    }

    #[test]
    fn requires_keyword_is_case_insensitive() {
        for kw in ["requires", "REQUIRES", "Requires"] {
            let input = format!("linear/qa-handoff/3.5.5 {kw} browserbase");
            assert!(PolicySuggestion::parse(&input).is_ok(), "kw: {kw}");
        }
    }

    #[test]
    fn empty_input_errors() {
        let err = PolicySuggestion::parse("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn missing_target_parts_errors() {
        let err = PolicySuggestion::parse("linear/qa-handoff requires browserbase").unwrap_err();
        // Error message names the actual part count it received.
        assert!(
            err.to_string().contains("skill/phase/step"),
            "error must explain the expected target shape: {err}"
        );
    }

    #[test]
    fn missing_requires_keyword_errors() {
        let err = PolicySuggestion::parse("linear/qa-handoff/3.5.5 must browserbase").unwrap_err();
        assert!(
            err.to_string().contains("'requires'"),
            "must mention the missing keyword: {err}"
        );
    }

    #[test]
    fn unknown_mode_errors() {
        let err = PolicySuggestion::parse(
            "linear/qa-handoff/3.5.5 requires browserbase strict",
        )
        .unwrap_err();
        assert!(err.to_string().contains("strict"), "{err}");
    }

    #[test]
    fn trailing_garbage_errors() {
        let err = PolicySuggestion::parse(
            "linear/qa-handoff/3.5.5 requires browserbase verified please",
        )
        .unwrap_err();
        assert!(err.to_string().contains("trailing"), "{err}");
    }

    #[test]
    fn illegal_chars_in_ident_error() {
        // Whitespace inside what looks like a target field gets caught
        // by the parser at one of two layers (either the target-shape
        // check sees "lin" without slashes, or the unexpected-trailing
        // check fires). Either way: invalid input MUST produce Err.
        let err =
            PolicySuggestion::parse("lin ear/qa/1 requires browserbase").unwrap_err();
        // We don't pin the exact wording — different inputs hit different
        // grammar layers — just confirm it's a structural complaint.
        let msg = err.to_string();
        assert!(
            msg.contains("skill/phase/step")
                || msg.contains("illegal")
                || msg.contains("trailing")
                || msg.contains("'requires'"),
            "error must be a structural complaint: {msg}"
        );
    }

    #[test]
    fn toml_fragment_includes_source_policy_comment() {
        let policy = "linear/qa-handoff/3.5.5 requires browserbase";
        let s = PolicySuggestion::parse(policy).unwrap();
        let frag = s.to_toml_fragment(policy);
        assert!(frag.contains("Source policy:"));
        assert!(frag.contains(policy));
        assert!(frag.contains("[[step_verifiers]]"));
        assert!(frag.contains("verified-only"));
        assert!(frag.contains("verified_only = true"));
    }

    #[test]
    fn toml_fragment_for_provenance_mode_says_audit() {
        let policy = "linear/qa-handoff/3.5.5 requires browserbase provenance";
        let s = PolicySuggestion::parse(policy).unwrap();
        let frag = s.to_toml_fragment(policy);
        assert!(frag.contains("provenance-only"));
        assert!(frag.contains("audit"));
        assert!(frag.contains("verified_only = false"));
    }

    #[test]
    fn toml_fragment_roundtrips_through_toml_parser() {
        // The emitted fragment must actually parse as TOML so a
        // human pasting it into a config doesn't get a syntax error.
        let s = PolicySuggestion::parse(
            "linear/qa-handoff/3.5.5 requires browserbase provenance",
        )
        .unwrap();
        let frag = s.to_toml_fragment("test policy");
        #[derive(serde::Deserialize)]
        struct Wrapper {
            step_verifiers: Vec<StepVerifierRequirement>,
        }
        let parsed: Wrapper = toml::from_str(&frag)
            .expect("emitted fragment must be valid TOML");
        assert_eq!(parsed.step_verifiers.len(), 1);
        assert_eq!(parsed.step_verifiers[0], s.requirement);
    }
}
