// SENTINEL-24 — GitHub gateway shelling out to the `gh` CLI.
//
// Server-only (uses `child_process`). The composition root must inject this
// behind a server-rendered route — do not import from a Client Component.
//
// First-cut: reviewer/finding counts come back zeroed with a TODO; the
// real hydration belongs to the analytics layer (SEN-18 logic) once it
// reads `pr-review.jsonl` directly.

import { spawn } from "node:child_process";

import type {
  GitHubGateway,
  PullRequest,
  ReviewComment,
  TimeRange,
} from "../ports";

interface GhPrListItem {
  readonly number: number;
  readonly title: string;
  readonly mergedAt: string | null;
  readonly author?: { login: string };
}

interface GhPrViewItem {
  readonly comments?: ReadonlyArray<{
    author?: { login?: string };
    body?: string;
    createdAt?: string;
  }>;
  readonly reviews?: ReadonlyArray<{
    author?: { login?: string };
    body?: string;
    submittedAt?: string;
  }>;
}

const BOT_LOGIN_RE = /bot|codex|coderabbit/i;

export class GhCliGitHubGateway implements GitHubGateway {
  /**
   * @param ghBin path to the `gh` binary. Defaults to `"gh"` so PATH lookup
   * wins; tests override.
   */
  constructor(private readonly ghBin: string = "gh") {}

  async getMergedPullRequests(
    window: TimeRange,
    repos: string[],
  ): Promise<PullRequest[]> {
    const out: PullRequest[] = [];
    for (const repo of repos) {
      const raw = await this.runGh([
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "merged",
        "--limit",
        "200",
        "--json",
        "number,title,mergedAt,author",
      ]);
      const items = parseJsonArray<GhPrListItem>(raw);
      for (const item of items) {
        if (!item.mergedAt) continue;
        const mergedAt = new Date(item.mergedAt);
        if (Number.isNaN(mergedAt.getTime())) continue;
        if (
          mergedAt.getTime() < window.start.getTime() ||
          mergedAt.getTime() >= window.end.getTime()
        ) {
          continue;
        }
        out.push({
          number: item.number,
          repo,
          title: item.title,
          mergedAt,
          reviewerLogins: [],
          // TODO(SEN-18): hydrate from pr-review.jsonl or by walking the
          // reviews+comments stream below. Keeping zeroed for SEN-24's
          // initial adapter scope so the type contract is satisfied.
          codexFindings: 0,
          codeRabbitFindings: 0,
        });
      }
    }
    return out;
  }

  async getReviewComments(prNumber: number, repo: string): Promise<ReviewComment[]> {
    const raw = await this.runGh([
      "pr",
      "view",
      String(prNumber),
      "--repo",
      repo,
      "--json",
      "comments,reviews",
    ]);
    let parsed: GhPrViewItem;
    try {
      parsed = JSON.parse(raw) as GhPrViewItem;
    } catch {
      return [];
    }
    const out: ReviewComment[] = [];
    for (const c of parsed.comments ?? []) {
      const author = c.author?.login ?? "<unknown>";
      const postedAt = c.createdAt ? new Date(c.createdAt) : null;
      if (!postedAt || Number.isNaN(postedAt.getTime())) continue;
      out.push({
        author,
        body: c.body ?? "",
        isBot: BOT_LOGIN_RE.test(author),
        postedAt,
      });
    }
    for (const r of parsed.reviews ?? []) {
      const author = r.author?.login ?? "<unknown>";
      const postedAt = r.submittedAt ? new Date(r.submittedAt) : null;
      if (!postedAt || Number.isNaN(postedAt.getTime())) continue;
      out.push({
        author,
        body: r.body ?? "",
        isBot: BOT_LOGIN_RE.test(author),
        postedAt,
      });
    }
    return out;
  }

  /** Run `gh` with stdout capture. Throws if exit != 0. */
  private runGh(args: string[]): Promise<string> {
    return new Promise((resolve, reject) => {
      // Windows `gh` is a `.cmd` wrapper around the real binary, and Node
      // can't spawn `.cmd` without a shell. Set shell on win32 only — POSIX
      // stays shell-less so arg quoting can't bite us.
      const child = spawn(this.ghBin, args, { shell: process.platform === "win32" });
      let stdout = "";
      let stderr = "";
      child.stdout?.on("data", (chunk) => {
        stdout += String(chunk);
      });
      child.stderr?.on("data", (chunk) => {
        stderr += String(chunk);
      });
      child.on("error", reject);
      child.on("close", (code) => {
        if (code === 0) resolve(stdout);
        else reject(new Error(`gh exited ${code}: ${stderr.trim() || "<no stderr>"}`));
      });
    });
  }
}

function parseJsonArray<T>(raw: string): T[] {
  try {
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as T[]) : [];
  } catch {
    return [];
  }
}
