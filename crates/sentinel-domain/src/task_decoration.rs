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
    Some(match s.chars().find(|c| ['🔴', '🟠', '🟡', '🟢'].contains(c))? {
        '🔴' => "P0".into(),
        '🟠' => "P1".into(),
        '🟡' => "P2".into(),
        '🟢' => "P3".into(),
        _ => return None,
    })
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
        for status in ["in_progress", "pending", "completed", "blocked", "cancelled"] {
            let g = status_glyph(status).unwrap().chars().next().unwrap();
            assert!(DECOR_EMOJI.contains(&g), "render glyph {g} missing from DECOR_EMOJI");
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
        assert_eq!(priority_from_decoration("[P1] 🔴 do it").as_deref(), Some("P1"));
        assert_eq!(priority_from_decoration("🔴 do it").as_deref(), Some("P0"));
        assert_eq!(priority_from_decoration("no decoration"), None);
    }
}
