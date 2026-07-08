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
        s = s.trim_start_matches(|c| DECOR_EMOJI.contains(&c)).trim_start();
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
        for status in ["in_progress", "pending", "completed", "blocked", "cancelled"] {
            let glyph = status_glyph(status).unwrap();
            let decorated = format!("{glyph} Do the work");
            assert_eq!(strip_decoration(&decorated), "Do the work", "{status}");
        }
    }
}
