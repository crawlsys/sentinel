//! Deterministic command-output compression — sentinel's native "RTK".
//!
//! Long-running AI coding sessions burn context window on noisy command
//! output: `cargo test`'s per-test `... ok` spam, `Compiling …` lines,
//! progress bars, duplicate `grep`/`find` hits. This module collapses that
//! noise **structurally and deterministically** (no LLM, no network) while
//! guaranteeing that every *signal* line survives verbatim — so the
//! verification gate, the proof chain, and a human reading the transcript
//! still see `test result:`, `error[E…]`, `FAILED`, panics, and warnings
//! exactly as emitted.
//!
//! ## The signal-preservation invariant
//!
//! [`is_signal_line`] defines a hard allow-list of patterns that are NEVER
//! dropped or altered, regardless of which per-command rule runs. This is the
//! safety contract: compression can only ever remove *noise*, never the lines
//! a downstream gate parses to decide pass/fail. Tests assert this directly.
//!
//! ## Losslessness for grep/find/ls
//!
//! Search and file-listing output (grep, rg, find, fd, ls) is treated as
//! **near-verbatim**: only trailing whitespace and runs of blank lines are
//! removed. Consecutive identical lines are deduplicated (a true repeat
//! carries zero information), but no hard line cap is applied — every unique
//! result line is preserved regardless of count. This fixes the previous
//! lossy behaviour where lines beyond position 100 were silently dropped.
//!
//! ## Pure
//!
//! Zero IO. `compress(command, raw_output) -> CompressionResult`. The CLI
//! (`sentinel compress`) runs the command and feeds stdout here; the
//! `output_compressor` `PreToolUse` hook routes qualifying commands through the
//! CLI. This module owns only the transformation.

/// Result of compressing one command's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionResult {
    /// The compressed output (signal lines verbatim, noise collapsed).
    pub compressed: String,
    /// Byte length of the original output.
    pub original_bytes: usize,
    /// Byte length of the compressed output.
    pub compressed_bytes: usize,
    /// Number of source lines removed/collapsed.
    pub lines_dropped: usize,
}

impl CompressionResult {
    /// Fraction of bytes removed, in `[0.0, 1.0]`. `0.0` when nothing was
    /// compressed (or the original was empty).
    #[must_use]
    pub fn savings_ratio(&self) -> f64 {
        if self.original_bytes == 0 {
            return 0.0;
        }
        let saved = self.original_bytes.saturating_sub(self.compressed_bytes);
        saved as f64 / self.original_bytes as f64
    }

    /// A passthrough result — output unchanged. Used when a command isn't
    /// compressible or compression would not help.
    #[must_use]
    pub fn passthrough(output: &str) -> Self {
        Self {
            compressed: output.to_string(),
            original_bytes: output.len(),
            compressed_bytes: output.len(),
            lines_dropped: 0,
        }
    }
}

/// Command families we apply tailored rules to. `Other` = passthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// `cargo test` / `cargo nextest`.
    CargoTest,
    /// `cargo build` / `cargo check` / `cargo clippy`.
    CargoBuildLike,
    /// `git status`.
    GitStatus,
    /// `grep` / `rg` / `findstr`.
    Grep,
    /// `find` / `fd` / `ls` / `dir`.
    FileList,
    /// Anything else — passthrough.
    Other,
}

/// Classify a shell command string into a [`CommandKind`]. Looks at the first
/// meaningful tokens; tolerant of leading `cd … &&`, env prefixes, and
/// `sentinel compress --` wrappers (so double-compression is a no-op).
#[must_use]
pub fn classify(command: &str) -> CommandKind {
    let lower = command.to_lowercase();
    // Strip a leading `sentinel compress -- ` wrapper if present so the inner
    // command is what we classify (defensive against double-wrapping).
    let lower = lower
        .split_once("compress --")
        .map_or(lower.as_str(), |(_, rest)| rest.trim());

    let has = |needle: &str| lower.contains(needle);

    if has("cargo test") || has("cargo nextest") {
        CommandKind::CargoTest
    } else if has("cargo build") || has("cargo check") || has("cargo clippy") {
        CommandKind::CargoBuildLike
    } else if has("git status") {
        CommandKind::GitStatus
    } else if starts_with_word(lower, "grep")
        || starts_with_word(lower, "rg")
        || has(" grep ")
        || has(" rg ")
        || has("findstr")
    {
        CommandKind::Grep
    } else if starts_with_word(lower, "find")
        || starts_with_word(lower, "fd")
        || starts_with_word(lower, "ls")
        || starts_with_word(lower, "dir")
        || has(" find ")
    {
        CommandKind::FileList
    } else {
        CommandKind::Other
    }
}

/// True when `s`, after skipping a leading `cd … && ` / env-var prefix, begins
/// with `word` as a whole token.
fn starts_with_word(s: &str, word: &str) -> bool {
    // Take the segment after the last `&&` (command chains) then trim.
    let seg = s.rsplit("&&").next().unwrap_or(s).trim_start();
    seg.split_whitespace()
        .next()
        .is_some_and(|first| first == word)
}

/// A line that must survive compression verbatim. The safety invariant.
///
/// Matches build/test/lint failures, panics, result summaries, warnings, and
/// fatal git errors — everything a verification gate or proof step parses.
/// Deliberately broad: a false positive (keeping a noise line) costs a few
/// bytes; a false negative (dropping a signal line) breaks a gate.
#[must_use]
pub fn is_signal_line(line: &str) -> bool {
    let t = line.trim_start();
    let lower = t.to_lowercase();
    // Rust/cargo: errors, the test-result summary, individual FAILED tests,
    // panics, and warnings.
    t.starts_with("error")            // `error:` / `error[E0277]:`
        || t.starts_with("warning:")
        || lower.contains("test result:")
        || t.contains("FAILED")
        || lower.contains("panicked at")
        || lower.starts_with("thread '")  // panic header
        // git / generic tooling fatals.
        || t.starts_with("fatal:")
        || lower.contains("error:")
        // Non-zero exit / failure markers some tools print.
        || lower.contains("failed to ")
        || lower.contains("could not compile")
}

/// Compress `raw` output for the given `command`.
///
/// Always preserves [`is_signal_line`] lines verbatim. Per-command rules only
/// collapse recognized noise; unrecognized commands pass through unchanged.
#[must_use]
pub fn compress(command: &str, raw: &str) -> CompressionResult {
    let original_bytes = raw.len();
    let kind = classify(command);

    let compressed = match kind {
        CommandKind::CargoTest => compress_cargo_test(raw),
        CommandKind::CargoBuildLike => compress_cargo_build_like(raw),
        CommandKind::GitStatus => compress_passthrough_trim(raw),
        // Grep and FileList: lossless dedup-only (no cap). Every unique result
        // line is preserved in input order. Only consecutive identical lines
        // (true repeats, zero information) are collapsed.
        CommandKind::Grep | CommandKind::FileList => compress_dedup_only(raw),
        CommandKind::Other => return CompressionResult::passthrough(raw),
    };

    let source_lines = raw.lines().count();
    let out_lines = compressed.lines().count();
    CompressionResult {
        compressed_bytes: compressed.len(),
        compressed,
        original_bytes,
        lines_dropped: source_lines.saturating_sub(out_lines),
    }
}

/// `cargo test`: keep all signal lines; collapse the `… ok` per-test spam to a
/// single count line; drop `Compiling …` / `Finished …` / `Running …` build
/// chatter and blank runs. The `test result:` summary and any `FAILED` /
/// `error` line is preserved by [`is_signal_line`].
fn compress_cargo_test(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut ok_run: usize = 0;

    let flush_ok = |out: &mut Vec<String>, ok_run: &mut usize| {
        if *ok_run > 0 {
            out.push(format!("… {ok_run} passing test(s) (ok) collapsed"));
            *ok_run = 0;
        }
    };

    for line in raw.lines() {
        let t = line.trim_end();
        if is_signal_line(t) {
            flush_ok(&mut out, &mut ok_run);
            out.push(t.to_string());
            continue;
        }
        // Per-test pass line: `test some::path ... ok`
        if t.trim_start().starts_with("test ") && t.ends_with("... ok") {
            ok_run += 1;
            continue;
        }
        if is_cargo_build_noise(t) || t.trim().is_empty() {
            continue;
        }
        flush_ok(&mut out, &mut ok_run);
        out.push(t.to_string());
    }
    flush_ok(&mut out, &mut ok_run);
    join_nonempty(&out)
}

/// `cargo build` / `check` / `clippy`: drop `Compiling …`/`Finished …` noise,
/// keep everything else (warnings + errors are signal and pass through).
fn compress_cargo_build_like(raw: &str) -> String {
    let out: Vec<String> = raw
        .lines()
        .map(str::trim_end)
        .filter(|t| is_signal_line(t) || !(is_cargo_build_noise(t) || t.trim().is_empty()))
        .map(ToString::to_string)
        .collect();
    join_nonempty(&out)
}

/// Trim trailing whitespace + drop blank lines; keep all content. For
/// `git status` and similar already-terse output.
fn compress_passthrough_trim(raw: &str) -> String {
    let out: Vec<String> = raw
        .lines()
        .map(str::trim_end)
        .filter(|t| !t.trim().is_empty())
        .map(ToString::to_string)
        .collect();
    join_nonempty(&out)
}

/// Lossless dedup for `grep`/`find`/`ls` style output.
///
/// Collapses only **consecutive identical lines** (true repeats that carry zero
/// extra information) while preserving every unique line in its original order.
/// No line cap is applied: every distinct search result is preserved regardless
/// of count, so a `grep -n` across a large codebase returns all matches.
///
/// This replaces the old `compress_dedup_cap` which silently dropped lines
/// beyond position 100, causing lossy and apparently non-deterministic output
/// (the exact set of lines that survived depended on which signal-line patterns
/// happened to match first, consuming cap slots before unique results were
/// seen).
fn compress_dedup_only(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<&str> = None;

    for line in raw.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            last = None; // reset dedup across blank runs
            continue;
        }
        // Only collapse consecutive identical lines (same content back-to-back).
        if last == Some(t) {
            continue;
        }
        out.push(t.to_string());
        last = Some(t);
    }
    join_nonempty(&out)
}

/// `cargo`/rustc build-progress chatter that carries no signal.
fn is_cargo_build_noise(t: &str) -> bool {
    let s = t.trim_start();
    s.starts_with("Compiling ")
        || s.starts_with("Finished ")
        || s.starts_with("Running ")
        || s.starts_with("Fresh ")
        || s.starts_with("Building ")
        || s.starts_with("Downloading ")
        || s.starts_with("Downloaded ")
        || s.starts_with("Updating ")
        || s == "running 0 tests"
}

fn join_nonempty(lines: &[String]) -> String {
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognizes_families() {
        assert_eq!(classify("cargo test --workspace"), CommandKind::CargoTest);
        assert_eq!(
            classify("cd /repo && cargo test -p foo"),
            CommandKind::CargoTest
        );
        assert_eq!(
            classify("cargo clippy --workspace"),
            CommandKind::CargoBuildLike
        );
        assert_eq!(
            classify("cargo build --release"),
            CommandKind::CargoBuildLike
        );
        assert_eq!(classify("git status --short"), CommandKind::GitStatus);
        assert_eq!(classify("grep -rn foo src"), CommandKind::Grep);
        assert_eq!(classify("find . -name '*.rs'"), CommandKind::FileList);
        assert_eq!(classify("echo hello"), CommandKind::Other);
    }

    #[test]
    fn double_wrap_classifies_inner_command() {
        assert_eq!(
            classify("sentinel compress -- cargo test --workspace"),
            CommandKind::CargoTest
        );
    }

    #[test]
    fn signal_lines_detected() {
        assert!(is_signal_line("error[E0308]: mismatched types"));
        assert!(is_signal_line("error: could not compile `foo`"));
        assert!(is_signal_line("warning: unused variable"));
        assert!(is_signal_line("test result: ok. 42 passed; 0 failed"));
        assert!(is_signal_line("test result: FAILED. 1 passed; 2 failed"));
        assert!(is_signal_line("test foo::bar ... FAILED"));
        assert!(is_signal_line("thread 'main' panicked at src/x.rs:1:1"));
        assert!(is_signal_line("fatal: not a git repository"));
        assert!(!is_signal_line("test foo::bar ... ok"));
        assert!(!is_signal_line("   Compiling sentinel v0.4.1"));
    }

    #[test]
    fn cargo_test_collapses_ok_spam_but_keeps_result_and_failures() {
        let raw = "\
   Compiling sentinel v0.4.1
    Finished test profile
     Running unittests src/lib.rs
running 5 tests
test a::ok1 ... ok
test a::ok2 ... ok
test a::ok3 ... ok
test b::bad ... FAILED
test a::ok4 ... ok

failures:
    b::bad
test result: FAILED. 4 passed; 1 failed; 0 ignored";
        let r = compress("cargo test --workspace", raw);
        // Signal lines survive verbatim.
        assert!(r
            .compressed
            .contains("test result: FAILED. 4 passed; 1 failed; 0 ignored"));
        assert!(r.compressed.contains("test b::bad ... FAILED"));
        // ok-spam collapsed, not enumerated.
        assert!(!r.compressed.contains("test a::ok1 ... ok"));
        assert!(r.compressed.contains("passing test(s) (ok) collapsed"));
        // Build chatter dropped.
        assert!(!r.compressed.contains("Compiling"));
        assert!(!r.compressed.contains("Finished"));
        // Net smaller.
        assert!(r.compressed_bytes < r.original_bytes);
        assert!(r.lines_dropped > 0);
    }

    #[test]
    fn cargo_test_all_green_collapses_to_summary() {
        let raw = "\
running 3 tests
test x ... ok
test y ... ok
test z ... ok
test result: ok. 3 passed; 0 failed; 0 ignored";
        let r = compress("cargo test", raw);
        assert!(r
            .compressed
            .contains("test result: ok. 3 passed; 0 failed; 0 ignored"));
        assert!(r.compressed.contains("3 passing test(s) (ok) collapsed"));
        assert!(r.savings_ratio() > 0.0);
    }

    #[test]
    fn cargo_build_keeps_errors_drops_compiling() {
        let raw = "\
   Compiling foo v0.1.0
   Compiling bar v0.2.0
error[E0432]: unresolved import
    Finished dev profile";
        let r = compress("cargo build", raw);
        assert!(r.compressed.contains("error[E0432]: unresolved import"));
        assert!(!r.compressed.contains("Compiling foo"));
        assert!(!r.compressed.contains("Finished"));
    }

    // --- Losslessness regression tests (the lossy-compression bug) ---

    /// Grep output with near-duplicate lines (shared prefix, different line
    /// numbers) must preserve EVERY line in order. This was the primary bug:
    /// lines beyond position 100 were silently dropped.
    #[test]
    fn grep_near_duplicate_lines_all_preserved_in_order() {
        // Simulate `grep -n "pub struct" large_file.rs`: each line has a
        // distinct line number, so no two adjacent lines are identical.
        let lines: Vec<String> = (1..=150)
            .map(|n| format!("src/lib.rs:{n}:pub struct Foo{n}"))
            .collect();
        let raw = lines.join("\n");

        let r = compress("grep -n 'pub struct' src/lib.rs", &raw);

        // Every unique line must survive.
        for n in 1..=150 {
            let expected = format!("src/lib.rs:{n}:pub struct Foo{n}");
            assert!(
                r.compressed.contains(&expected),
                "line {n} was dropped: {expected}"
            );
        }
        // Output preserves input order.
        let out_lines: Vec<&str> = r.compressed.lines().collect();
        assert_eq!(
            out_lines.len(),
            150,
            "expected 150 lines, got {}",
            out_lines.len()
        );
        for (i, line) in out_lines.iter().enumerate() {
            let n = i + 1;
            assert!(
                line.contains(&format!("Foo{n}")),
                "line at position {i} out of order: {line}"
            );
        }
    }

    /// True consecutive duplicates (e.g. a grep that repeats the same match
    /// multiple times in a row) are still collapsed — that's the one lossless
    /// reduction we keep.
    #[test]
    fn grep_true_consecutive_duplicates_collapsed() {
        let raw =
            "src/lib.rs:42:match found\nsrc/lib.rs:42:match found\nsrc/lib.rs:43:other match\n";
        let r = compress("grep -rn match src", raw);
        // The duplicate is collapsed to one occurrence.
        let count = r.compressed.matches("src/lib.rs:42:match found").count();
        assert_eq!(
            count, 1,
            "consecutive dup should collapse to 1, got {count}"
        );
        // But the distinct line is still present.
        assert!(r.compressed.contains("src/lib.rs:43:other match"));
    }

    /// A `find` result with >100 unique paths must preserve all of them.
    #[test]
    fn find_more_than_100_paths_all_preserved() {
        let lines: Vec<String> = (1..=120)
            .map(|n| format!("./src/module{n}/lib.rs"))
            .collect();
        let raw = lines.join("\n");
        let r = compress("find . -name '*.rs'", &raw);
        assert_eq!(
            r.compressed.lines().count(),
            120,
            "all 120 find results must be preserved"
        );
        for n in 1..=120 {
            assert!(
                r.compressed.contains(&format!("module{n}")),
                "path for module{n} was dropped"
            );
        }
    }

    /// `ls` output with >100 files must preserve all entries.
    #[test]
    fn ls_more_than_100_files_all_preserved() {
        let lines: Vec<String> = (1..=110).map(|n| format!("file{n:03}.rs")).collect();
        let raw = lines.join("\n");
        let r = compress("ls src/", &raw);
        assert_eq!(
            r.compressed.lines().count(),
            110,
            "all 110 ls entries must be preserved"
        );
    }

    /// The compressor must be deterministic: same input always produces the
    /// same output, regardless of how many times it is called.
    #[test]
    fn compression_is_deterministic() {
        let lines: Vec<String> = (1..=80)
            .map(|n| format!("src/lib.rs:{n}:pub struct S{n}"))
            .collect();
        let raw = lines.join("\n");

        let first = compress("grep -n 'pub struct' src", &raw);
        for i in 1..=10 {
            let repeated = compress("grep -n 'pub struct' src", &raw);
            assert_eq!(
                first.compressed, repeated.compressed,
                "compression not deterministic on run {i}"
            );
        }
    }

    /// A `cat` / `Read` of source code must be byte-identical (modulo trailing
    /// whitespace on each line) since `cat` classifies as `Other` → passthrough.
    #[test]
    fn cat_command_is_passthrough() {
        let raw = "fn main() {\n    println!(\"hello\");\n}\n";
        let r = compress("cat src/main.rs", raw);
        assert_eq!(r.compressed, raw, "cat output must pass through unchanged");
        assert_eq!(r.lines_dropped, 0);
    }

    #[test]
    fn grep_output_same_input_same_output() {
        // Regression for the non-determinism symptom: same grep output must
        // always compress to the same result.
        let raw = (1..=50)
            .map(|n| format!("{n}: pub struct Foo"))
            .collect::<Vec<_>>()
            .join("\n");
        let a = compress("grep -n 'pub struct' file.rs", &raw);
        let b = compress("grep -n 'pub struct' file.rs", &raw);
        assert_eq!(a.compressed, b.compressed);
        assert_eq!(a.lines_dropped, b.lines_dropped);
    }

    #[test]
    fn other_command_is_passthrough() {
        let raw = "arbitrary output\nsecond line";
        let r = compress("echo hello", raw);
        assert_eq!(r.compressed, raw);
        assert_eq!(r.lines_dropped, 0);
        assert!((r.savings_ratio() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_output_is_safe() {
        let r = compress("cargo test", "");
        assert_eq!(r.compressed, "");
        assert!((r.savings_ratio() - 0.0).abs() < f64::EPSILON);
    }

    /// Old grep_dedups_and_caps test updated: consecutive dups still collapse,
    /// but there is no longer a hard 100-line cap that drops unique lines.
    #[test]
    fn grep_consecutive_dups_collapse_no_lossy_cap() {
        // 150 lines all identical → collapse to 1 (consecutive dedup).
        let mut raw = String::new();
        for _ in 0..150 {
            raw.push_str("src/lib.rs: match found\n");
        }
        let r = compress("grep -rn match src", &raw);
        // Collapsed to a single line (all are consecutive identical).
        assert_eq!(
            r.compressed.lines().count(),
            1,
            "150 identical consecutive lines should collapse to 1"
        );
        assert!(r.compressed_bytes < r.original_bytes);
    }
}
