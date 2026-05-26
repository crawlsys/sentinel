/// Text-density helpers for the ticker and inspector. The 360px
/// ticker column is unforgiving — every char counts, and absolute
/// paths like `/home/kcrawley/projects/basilisk/.worktrees/...`
/// eat the whole row before getting to the actual command.
///
/// Each function is pure + tiny + tested. Compose them at the
/// rendering site; don't build a hidden pipeline.

/// Replace `/home/<user>/` with `~/`. Generic over any unix user
/// since the bridge runs across machines.
///   /home/kcrawley/projects/basilisk → ~/projects/basilisk
export function tildify(text: string): string {
  if (!text) return text;
  return text.replace(/\/home\/[a-z][a-z0-9_-]*\//gi, "~/");
}

/// Strip a leading `cd <path>; ` or `cd <path> && ` chain. Used
/// for Bash commands where the operator's first move is always
/// `cd` into the worktree — the cd itself is noise once you've
/// identified the session.
///
///   cd /home/kcrawley/projects/basilisk; git status → git status
///   cd ./tools && cargo test                       → cargo test
///   plain echo hello                                → plain echo hello
export function stripCdPrefix(text: string): string {
  if (!text) return text;
  return text.replace(/^cd\s+[^\s;&]+\s*(?:;|&&)\s*/, "");
}

/// END-truncate: keep the tail of a string visible, hide the head.
/// Useful for paths where the filename matters more than the
/// prefix.  Default 72 chars (fits the 360px column at 10px mono).
///
///   tools/sentinel-viz-next/components/EventTicker.tsx →
///       …components/EventTicker.tsx
export function truncTail(text: string, max = 72): string {
  if (!text) return text;
  if (text.length <= max) return text;
  return `…${text.slice(-max + 1)}`;
}

/// Smart truncate: end-truncate when text looks like a path
/// (contains a `/` and no spaces in the last segment), middle-
/// truncate when text looks like a free-form command (has spaces
/// or pipes). The intuition is operators care about WHICH file
/// for paths but WHAT WAS DONE for commands.
export function smartTrunc(text: string, max = 72): string {
  if (!text) return text;
  if (text.length <= max) return text;
  const isPath = /\//.test(text) && !/\s/.test(text);
  if (isPath) return truncTail(text, max);
  // Middle truncate — keep the start (command name) AND the end
  // (typically the target file / argument).
  const half = Math.floor((max - 1) / 2);
  return `${text.slice(0, half)}…${text.slice(-(max - 1 - half))}`;
}

/// Pipeline for Bash commands: strip the cd prefix, tildify the
/// remaining paths, then smart-truncate to the column width.
export function compactBashCommand(text: string, max = 72): string {
  return smartTrunc(tildify(stripCdPrefix(text)), max);
}

/// Pipeline for Edit/Read/Write file paths: tildify, end-truncate.
export function compactPath(text: string, max = 72): string {
  return truncTail(tildify(text), max);
}

/// Parse a git-diff-stats footer out of a Bash command's result
/// preview. Recognises the two common formats:
///   3 files changed, 25 insertions(+), 5 deletions(-)
///   1 file changed, 12 insertions(+)
///   2 files changed, 8 deletions(-)
/// Returns null if no stats found.
export interface GitDiffStats {
  files: number;
  insertions: number;
  deletions: number;
}
export function parseGitDiffStats(resultPreview: string | null | undefined): GitDiffStats | null {
  if (!resultPreview) return null;
  const m = /(\d+)\s+files?\s+changed(?:,\s*(\d+)\s+insertions?\(\+\))?(?:,\s*(\d+)\s+deletions?\(-\))?/.exec(
    resultPreview,
  );
  if (!m) return null;
  return {
    files: parseInt(m[1] ?? "0", 10),
    insertions: parseInt(m[2] ?? "0", 10),
    deletions: parseInt(m[3] ?? "0", 10),
  };
}

/// Format diff stats as a compact chip-friendly string:
///   +25/-5 · 3 files
///   +12 · 1 file
///   -8 · 2 files
export function formatGitDiffStats(stats: GitDiffStats): string {
  const parts: string[] = [];
  if (stats.insertions > 0) parts.push(`+${stats.insertions}`);
  if (stats.deletions > 0) parts.push(`-${stats.deletions}`);
  const head = parts.join("/") || "·";
  const filesLabel = stats.files === 1 ? "1 file" : `${stats.files} files`;
  return `${head} · ${filesLabel}`;
}
