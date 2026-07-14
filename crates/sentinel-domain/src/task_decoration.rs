//! Canonical task status/priority decoration vocabulary.
//!
//! Several hooks render task subjects with a leading status/priority glyph and
//! several others parse that glyph back off — they are render/parse inverses,
//! and historically each hook carried its own copy of the glyph table. A glyph
//! added to one renderer but not the matching stripper silently breaks
//! round-tripping. This module is the single source of truth: the `DECOR_EMOJI`
//! set both strippers use, plus the `status ⇄ glyph` and `priority ⇄ glyph`
//! mappings. Pure — only `char`/`str` mapping, no IO.

/// Every decoration glyph a subject may lead with — status (`🔄⏳✅❌🚫`) and
/// priority (`🔴🟠🟡🟢`) colours. Strippers trim a leading run of these.
pub const DECOR_EMOJI: &[char] = &['🔄', '⏳', '✅', '❌', '🚫', '🔴', '🟠', '🟡', '🟢'];

/// Glyph for a task status word (the render direction). Returns `None` for an
/// unrecognised status so callers can fall back to the bare word.
#[must_use]
pub fn status_glyph(status: &str) -> Option<&'static str> {
    Some(match status {
        "in_progress" => "🔄",
        "pending" => "⏳",
        "completed" => "✅",
        "blocked" => "🚫",
        "cancelled" | "canceled" | "deleted" => "❌",
        _ => return None,
    })
}

/// `"🔄 in_progress"` style decorated status, or the bare status when
/// unrecognised.
#[must_use]
pub fn decorated_status(status: &str) -> String {
    status_glyph(status).map_or_else(|| status.to_string(), |g| format!("{g} {status}"))
}

/// Compose the canonical decorated subject from a task's fields: a leading
/// glyph run — status, then priority colour, then a `🚫` blocked marker — in
/// front of the cleaned subject. This is the single render used by BOTH the
/// on-disk subject decorator and the status-line renderer, so a subject can be
/// decorated once, re-decorated idempotently (the input is always stripped
/// first), and never accretes doubled glyphs.
///
/// - `status`: native status word (`pending`/`in_progress`/`completed`); an
///   unknown status contributes no status glyph.
/// - `priority`: optional `metadata.priority` value (see [`priority_glyph`] for
///   accepted forms); `None` or unrecognised contributes no colour.
/// - `blocked`: true when the task has a non-empty `blockedBy` — adds `🚫`.
///   (Native status has no "blocked" variant, so this is derived separately.)
///
/// The returned string is `strip_decoration`-clean apart from the leading run
/// this function adds, so `strip_decoration(decorate_subject(..)) == clean`.
#[must_use]
pub fn decorate_subject(
    subject: &str,
    status: &str,
    priority: Option<&str>,
    blocked: bool,
) -> String {
    let clean = strip_decoration(subject);
    let mut prefix = String::new();
    if let Some(g) = status_glyph(status) {
        prefix.push_str(g);
    }
    if let Some(g) = priority.and_then(priority_glyph) {
        prefix.push_str(g);
    }
    // Only add a standalone blocked marker when the status glyph isn't already
    // the blocked glyph, to avoid `🚫🚫`.
    if blocked && status_glyph(status) != Some("🚫") {
        prefix.push('🚫');
    }
    if prefix.is_empty() {
        clean.to_string()
    } else {
        format!("{prefix} {clean}")
    }
}

/// Strip leading status/priority decoration a caller (or a previous decorate
/// pass) baked into a task subject, returning just the description. Trims, in a
/// loop until stable: a leading run of `DECOR_EMOJI` (status **and** priority
/// colours, including `🚫`), a `[P0]`..`[P3]` token, a bare leading numeric
/// rank, and a leading `—`/`-`/`:` separator. Idempotent, and a no-op for a
/// clean subject. This is the single canonical stripper — the render/parse
/// inverse of [`status_glyph`]/[`decorated_status`] — so a status-emoji baked
/// onto a native task subject can be cleanly removed and re-applied when the
/// status changes (no stacking, no stale glyph).
#[must_use]
pub fn strip_decoration(subject: &str) -> &str {
    let mut s = subject.trim_start();
    loop {
        let before = s;
        // Leading decoration emoji (status + priority colours).
        s = s
            .trim_start_matches(|c| DECOR_EMOJI.contains(&c))
            .trim_start();
        // Leading [Pn] priority token.
        if let Some(rest) = s.strip_prefix('[') {
            if let Some(close) = rest.find(']') {
                let inner = &rest[..close];
                if inner.len() <= 3
                    && inner.starts_with('P')
                    && inner[1..].chars().all(|c| c.is_ascii_digit())
                {
                    s = rest[close + 1..].trim_start();
                }
            }
        }
        // Bare leading numeric rank (e.g. the "1" in "🔄 1 [P0]").
        let trimmed_num = s.trim_start_matches(|c: char| c.is_ascii_digit());
        if trimmed_num.len() < s.len() && trimmed_num.starts_with([' ', '—', '-', ':']) {
            s = trimmed_num.trim_start();
        }
        // Leading separator.
        s = s.trim_start_matches(['—', '-', ':']).trim_start();
        if s == before {
            break;
        }
    }
    s
}

/// Status word from a leading glyph (the parse direction). The inverse of
/// [`status_glyph`] for the glyphs that map 1:1 back to a canonical word.
#[must_use]
pub fn status_from_glyph(subject: &str) -> Option<&'static str> {
    Some(match subject.trim_start().chars().next()? {
        '🔄' => "in_progress",
        '⏳' => "pending",
        '✅' => "completed",
        '❌' => "cancelled",
        _ => return None,
    })
}

/// Colour glyph for a priority (the render direction — inverse of the colour
/// branch of [`priority_from_decoration`]). Accepts the canonical `P0`..`P3`
/// tokens plus the common word aliases task tooling emits into `metadata`
/// (`urgent`/`critical`→P0, `high`→P1, `medium`/`normal`→P2, `low`→P3), and the
/// raw Linear-style numeric priority `1`..`4`. Case-insensitive. Returns `None`
/// for an unrecognised value so callers render no colour rather than a wrong one.
///
/// `🔴`=P0 (urgent), `🟠`=P1 (high), `🟡`=P2 (medium), `🟢`=P3 (low).
#[must_use]
pub fn priority_glyph(priority: &str) -> Option<&'static str> {
    let p = priority.trim().to_ascii_lowercase();
    Some(match p.as_str() {
        "p0" | "0" | "1" | "urgent" | "critical" | "highest" => "🔴",
        "p1" | "2" | "high" => "🟠",
        "p2" | "3" | "medium" | "normal" | "med" => "🟡",
        "p3" | "4" | "low" | "lowest" => "🟢",
        _ => return None,
    })
}

/// `"P0".."P3"` from a leading `[Pn]` token (preferred) or colour glyph.
/// `🔴`=P0, `🟠`=P1, `🟡`=P2, `🟢`=P3.
#[must_use]
pub fn priority_from_decoration(subject: &str) -> Option<String> {
    let s = subject.trim_start();
    for tok in ["[P0]", "[P1]", "[P2]", "[P3]"] {
        if s.contains(tok) {
            return Some(tok.trim_matches(['[', ']']).to_string());
        }
    }
    Some(
        match s.chars().find(|c| ['🔴', '🟠', '🟡', '🟢'].contains(c))? {
            '🔴' => "P0".into(),
            '🟠' => "P1".into(),
            '🟡' => "P2".into(),
            '🟢' => "P3".into(),
            _ => return None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_parse_status_round_trip() {
        for status in ["in_progress", "pending", "completed", "cancelled"] {
            let decorated = decorated_status(status);
            assert_eq!(status_from_glyph(&decorated), Some(status));
        }
    }

    #[test]
    fn decor_emoji_covers_every_render_glyph() {
        // The stripper set MUST contain every glyph a renderer can emit, or
        // strip stops cleaning that glyph. This is the exact drift the shared
        // module prevents.
        for status in [
            "in_progress",
            "pending",
            "completed",
            "blocked",
            "cancelled",
        ] {
            let g = status_glyph(status).unwrap().chars().next().unwrap();
            assert!(
                DECOR_EMOJI.contains(&g),
                "render glyph {g} missing from DECOR_EMOJI"
            );
        }
        for g in ['🔴', '🟠', '🟡', '🟢'] {
            assert!(DECOR_EMOJI.contains(&g), "priority glyph {g} missing");
        }
    }

    #[test]
    fn unknown_status_falls_back_to_bare_word() {
        assert_eq!(status_glyph("weird"), None);
        assert_eq!(decorated_status("weird"), "weird");
    }

    #[test]
    fn priority_token_beats_glyph() {
        assert_eq!(
            priority_from_decoration("[P1] 🔴 do it").as_deref(),
            Some("P1")
        );
        assert_eq!(priority_from_decoration("🔴 do it").as_deref(), Some("P0"));
        assert_eq!(priority_from_decoration("no decoration"), None);
    }

    #[test]
    fn strip_decoration_cases() {
        // Status + priority glyph + numeric rank + [Pn] token + separator.
        assert_eq!(
            strip_decoration("🔄 🔴 1 [P0] — Fix memory-capture gate"),
            "Fix memory-capture gate"
        );
        assert_eq!(strip_decoration("✅ Ship the thing"), "Ship the thing");
        assert_eq!(strip_decoration("[P1] Do the work"), "Do the work");
        assert_eq!(strip_decoration("2 - build it"), "build it");
        // The 🚫 (blocked) glyph the session_init copy used to miss.
        assert_eq!(strip_decoration("🚫 Blocked task"), "Blocked task");
        // Clean subject is a no-op; stripping is idempotent.
        assert_eq!(
            strip_decoration("Restore mcpServers registrations"),
            "Restore mcpServers registrations"
        );
        let once = strip_decoration("🔴 [P0] — X");
        assert_eq!(strip_decoration(once), once, "idempotent");
    }

    #[test]
    fn decorate_then_strip_round_trips_to_clean() {
        // The core native-decorator invariant: strip(glyph + clean) == clean,
        // for every status, so re-decoration never accretes.
        for status in [
            "in_progress",
            "pending",
            "completed",
            "blocked",
            "cancelled",
        ] {
            let glyph = status_glyph(status).unwrap();
            let decorated = format!("{glyph} Do the work");
            assert_eq!(strip_decoration(&decorated), "Do the work", "{status}");
        }
    }

    #[test]
    fn priority_glyph_accepts_tokens_words_and_numbers() {
        assert_eq!(priority_glyph("P0"), Some("🔴"));
        assert_eq!(priority_glyph("urgent"), Some("🔴"));
        assert_eq!(priority_glyph("1"), Some("🔴")); // Linear numeric
        assert_eq!(priority_glyph("HIGH"), Some("🟠")); // case-insensitive
        assert_eq!(priority_glyph("medium"), Some("🟡"));
        assert_eq!(priority_glyph("p3"), Some("🟢"));
        assert_eq!(priority_glyph("low"), Some("🟢"));
        assert_eq!(priority_glyph("nonsense"), None);
        assert_eq!(priority_glyph(""), None);
        // Forward/back consistency: the glyph priority_glyph emits maps back to
        // the same Pn via priority_from_decoration.
        for (word, pn) in [
            ("urgent", "P0"),
            ("high", "P1"),
            ("medium", "P2"),
            ("low", "P3"),
        ] {
            let g = priority_glyph(word).unwrap();
            assert_eq!(priority_from_decoration(g).as_deref(), Some(pn), "{word}");
        }
    }

    #[test]
    fn decorate_subject_composes_status_priority_blocked() {
        // status only
        assert_eq!(
            decorate_subject("Do it", "in_progress", None, false),
            "🔄 Do it"
        );
        // status + priority
        assert_eq!(
            decorate_subject("Do it", "pending", Some("high"), false),
            "⏳🟠 Do it"
        );
        // status + priority + blocked
        assert_eq!(
            decorate_subject("Do it", "pending", Some("P0"), true),
            "⏳🔴🚫 Do it"
        );
        // blocked but no known status/priority → just the marker
        assert_eq!(decorate_subject("Do it", "weird", None, true), "🚫 Do it");
        // nothing to add → clean subject unchanged
        assert_eq!(decorate_subject("Do it", "weird", None, false), "Do it");
    }

    #[test]
    fn decorate_subject_is_idempotent_and_strips_clean() {
        // Re-decorating an already-decorated subject must not accrete glyphs,
        // and stripping the result recovers the clean subject — this is the
        // invariant that kills the doubled-glyph bug by construction.
        let once = decorate_subject("Fix the gate", "in_progress", Some("P1"), true);
        let twice = decorate_subject(&once, "in_progress", Some("P1"), true);
        assert_eq!(once, twice, "re-decoration is idempotent");
        assert_eq!(strip_decoration(&once), "Fix the gate");
        // Even if the status/priority CHANGE, re-decorating from the old
        // decorated form yields only the new decoration (old glyphs stripped).
        let changed = decorate_subject(&once, "completed", None, false);
        assert_eq!(changed, "✅ Fix the gate");
    }
}
