// SENTINEL-23 — GitHub gateway port.
//
// Read-side access to GitHub data (merged PRs + review comments). Adapters
// wrap the gh CLI / REST API behind this contract so the application
// layer stays GitHub-implementation-agnostic.
//
// Pure interface declarations only — zero implementations, zero IO.

import type { PullRequest, ReviewComment } from "./types";
import type { TimeRange } from "./time-range";

export interface GitHubGateway {
  getMergedPullRequests(window: TimeRange, repos: string[]): Promise<PullRequest[]>;
  getReviewComments(prNumber: number, repo: string): Promise<ReviewComment[]>;
}
