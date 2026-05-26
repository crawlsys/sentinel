import { describe, it, expect } from "vitest";

import {
  compactBashCommand,
  compactPath,
  formatGitDiffStats,
  parseGitDiffStats,
  smartTrunc,
  stripCdPrefix,
  tildify,
  truncTail,
} from "../../lib/format-text";

describe("tildify", () => {
  it("collapses /home/<user>/ to ~/", () => {
    expect(tildify("/home/kcrawley/projects/basilisk")).toBe("~/projects/basilisk");
    expect(tildify("/home/alice/work")).toBe("~/work");
  });

  it("collapses MULTIPLE home occurrences", () => {
    expect(tildify("cp /home/kcrawley/a /home/kcrawley/b")).toBe("cp ~/a ~/b");
  });

  it("leaves non-home paths alone", () => {
    expect(tildify("/etc/passwd")).toBe("/etc/passwd");
    expect(tildify("/var/log/syslog")).toBe("/var/log/syslog");
  });

  it("handles empty + falsy strings", () => {
    expect(tildify("")).toBe("");
  });
});

describe("stripCdPrefix", () => {
  it("strips `cd path; ` chains", () => {
    expect(stripCdPrefix("cd /home/kcrawley/projects/x; git status")).toBe("git status");
  });

  it("strips `cd path && ` chains", () => {
    expect(stripCdPrefix("cd ./tools && cargo test")).toBe("cargo test");
  });

  it("leaves commands without a leading cd alone", () => {
    expect(stripCdPrefix("git status")).toBe("git status");
    expect(stripCdPrefix("echo cd foo")).toBe("echo cd foo");
  });
});

describe("truncTail / smartTrunc", () => {
  it("truncTail keeps the END of long strings (filename-preserving)", () => {
    const out = truncTail(
      "tools/sentinel-viz-next/components/EventTicker.tsx",
      30,
    );
    expect(out.endsWith("EventTicker.tsx")).toBe(true);
    expect(out.startsWith("…")).toBe(true);
  });

  it("truncTail returns the input unchanged when under the limit", () => {
    expect(truncTail("short", 10)).toBe("short");
  });

  it("smartTrunc end-truncates when input looks like a path", () => {
    const out = smartTrunc(
      "a/very/very/long/nested/path/to/some/deeply/buried/file.tsx",
      20,
    );
    expect(out.endsWith("file.tsx")).toBe(true);
    expect(out.startsWith("…")).toBe(true);
  });

  it("smartTrunc middle-truncates when input has spaces (looks like a command)", () => {
    const out = smartTrunc(
      "git commit -m 'Add a feature with a deliberately long message ending here'",
      40,
    );
    expect(out.includes("…")).toBe(true);
    // Both start AND end should be visible.
    expect(out.startsWith("git commit")).toBe(true);
    expect(out.endsWith("here'")).toBe(true);
  });
});

describe("compactBashCommand", () => {
  it("strips cd prefix + tildifies + truncates", () => {
    const out = compactBashCommand(
      "cd /home/kcrawley/projects/basilisk/.worktrees/feat-burst-direct; git push -u origin feat/burst-direct-remediation",
      72,
    );
    expect(out).not.toContain("cd ");
    expect(out).not.toContain("/home/kcrawley");
    expect(out).toContain("git push");
  });

  it("handles a bare command (no cd) unchanged through the tildify path", () => {
    expect(compactBashCommand("git status --short")).toBe("git status --short");
  });
});

describe("compactPath", () => {
  it("tildifies + end-truncates so the filename stays visible", () => {
    const out = compactPath(
      "/home/kcrawley/projects/basilisk/.worktrees/long-name/tools/sentinel-viz-next/components/EventTicker.tsx",
      30,
    );
    expect(out).not.toContain("/home/kcrawley");
    expect(out.endsWith("EventTicker.tsx")).toBe(true);
  });
});

describe("parseGitDiffStats", () => {
  it("parses the standard 'N files changed, A insertions(+), B deletions(-)' footer", () => {
    const s = parseGitDiffStats(
      "[feat/x abc1234] my commit\n 3 files changed, 25 insertions(+), 5 deletions(-)",
    );
    expect(s).toEqual({ files: 3, insertions: 25, deletions: 5 });
  });

  it("parses the insertions-only variant", () => {
    const s = parseGitDiffStats(" 1 file changed, 12 insertions(+)");
    expect(s).toEqual({ files: 1, insertions: 12, deletions: 0 });
  });

  it("parses the deletions-only variant", () => {
    const s = parseGitDiffStats(" 2 files changed, 8 deletions(-)");
    expect(s).toEqual({ files: 2, insertions: 0, deletions: 8 });
  });

  it("returns null when no stats line is present", () => {
    expect(parseGitDiffStats("[abc] commit message")).toBeNull();
    expect(parseGitDiffStats(null)).toBeNull();
    expect(parseGitDiffStats("")).toBeNull();
  });
});

describe("formatGitDiffStats", () => {
  it("formats +A/-B · N files", () => {
    expect(formatGitDiffStats({ files: 3, insertions: 25, deletions: 5 })).toBe("+25/-5 · 3 files");
  });

  it("formats single-file as '1 file' (not '1 files')", () => {
    expect(formatGitDiffStats({ files: 1, insertions: 12, deletions: 0 })).toBe("+12 · 1 file");
  });

  it("handles deletion-only commits", () => {
    expect(formatGitDiffStats({ files: 2, insertions: 0, deletions: 8 })).toBe("-8 · 2 files");
  });
});
