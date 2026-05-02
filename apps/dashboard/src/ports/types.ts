// SENTINEL-23 — Shared port-level DTOs.
//
// External-system payload shapes that don't carry their own domain rules.
// Adapters fetch raw data from APIs / webhooks / state stores and hand
// these out to the application layer. Anything with rich invariants
// belongs in `src/domain/`, not here.

import type { TicketIdentifier } from "../domain";

/**
 * A single Claude session's token usage + cost, optionally tagged with the
 * Linear ticket it was working on.
 */
export interface TokenSession {
  readonly ticketId?: TicketIdentifier;
  readonly sessionId: string;
  readonly totalInput: number;
  readonly cacheRead: number;
  readonly cacheCreation: number;
  readonly output: number;
  readonly costUsd: number;
  readonly model: string;
}

/** Severity tier for a recorded incident. */
export type IncidentSeverity = "sev1" | "sev2" | "sev3" | "sev4";

/** A production incident (or recoverable event) tied to a Linear ticket. */
export interface Incident {
  readonly ticketId: TicketIdentifier;
  readonly createdAt: Date;
  readonly completedAt?: Date;
  readonly severity: IncidentSeverity;
}

/** A merged GitHub pull request, enriched with reviewer + bot finding counts. */
export interface PullRequest {
  readonly number: number;
  readonly repo: string;
  readonly title: string;
  readonly mergedAt: Date;
  readonly reviewerLogins: readonly string[];
  readonly codexFindings: number;
  readonly codeRabbitFindings: number;
}

/** A single review comment on a PR (human or bot). */
export interface ReviewComment {
  readonly author: string;
  readonly body: string;
  readonly isBot: boolean;
  readonly postedAt: Date;
}
