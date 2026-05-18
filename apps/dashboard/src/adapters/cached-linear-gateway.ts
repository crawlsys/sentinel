// SENTINEL-24 — Linear gateway backed by the per-project assigned cache.
//
// Reads `~/.claude/sentinel/linear-assigned-{project}.json` — a JSON array
// populated by the existing Linear refresh cron (no live MCP roundtrip).
// Missing cache file → empty results (the Linear cron may not have run
// yet, or no project is configured).

import { promises as fs } from "node:fs";
import path from "node:path";

import {
  makeTeam,
  makeTicketIdentifier,
  type Priority,
  type Team,
  type Ticket,
} from "../domain";
import type { LinearGateway, TimeRange } from "../ports";

type LinearStatusType =
  | "started"
  | "unstarted"
  | "backlog"
  | "triage"
  | "completed"
  | "canceled"
  | string;

/** Cache row shape — mirrors `LinearIssue` in task_persist.rs. */
interface LinearCacheRow {
  readonly identifier: string;
  readonly title?: string;
  readonly status_type?: LinearStatusType;
  readonly state?: string;
  readonly priority?: number | string;
  readonly estimate?: number;
  readonly url?: string;
  readonly created_at?: string;
  readonly completed_at?: string;
}

const ACTIVE_STATUSES: ReadonlySet<string> = new Set([
  "started",
  "unstarted",
  "backlog",
  "triage",
]);

const TICKET_ID_RE = /^[A-Z]+-\d+$/;

export class CachedLinearGateway implements LinearGateway {
  /** Absolute path to the cache file. */
  private readonly cachePath: string;

  /**
   * @param homeDir absolute home directory.
   * @param projectName project slug (e.g. `firefly-pro`). Used to pick the
   * matching `linear-assigned-{project}.json` file.
   */
  constructor(homeDir: string, projectName: string) {
    this.cachePath = path.join(
      homeDir,
      ".claude",
      "sentinel",
      `linear-assigned-${projectName}.json`,
    );
  }

  async getActiveTickets(): Promise<Ticket[]> {
    const rows = await this.readCache();
    const out: Ticket[] = [];
    for (const row of rows) {
      if (!ACTIVE_STATUSES.has(row.status_type ?? "")) continue;
      const ticket = rowToTicket(row);
      if (ticket) out.push(ticket);
    }
    return out;
  }

  async getCompletedTickets(_window: TimeRange): Promise<Ticket[]> {
    // TODO(SEN-2): the assigned-cache only carries currently-assigned issues
    // and doesn't snapshot completion history. Wire to a Linear-history
    // collector (likely the same one feeding SEN-17 per-stage cycle time).
    return [];
  }

  async getTeams(): Promise<Team[]> {
    const tickets = await this.getActiveTickets();
    const seen = new Set<string>();
    for (const t of tickets) {
      const prefix = String(t.id).split("-")[0];
      if (prefix) seen.add(prefix);
    }
    return Array.from(seen, (slug) => makeTeam(slug));
  }

  private async readCache(): Promise<LinearCacheRow[]> {
    let raw: string;
    try {
      raw = await fs.readFile(this.cachePath, "utf8");
    } catch (e) {
      if (isEnoent(e)) return [];
      throw e;
    }
    try {
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? (parsed as LinearCacheRow[]) : [];
    } catch {
      return [];
    }
  }
}

function rowToTicket(row: LinearCacheRow): Ticket | null {
  if (!TICKET_ID_RE.test(row.identifier)) return null;
  let id;
  try {
    id = makeTicketIdentifier(row.identifier);
  } catch {
    return null;
  }
  const teamSlug = row.identifier.split("-")[0];
  if (!teamSlug) return null;
  const team = makeTeam(teamSlug);
  const priority = priorityFromCacheValue(row.priority);
  const state = row.state ?? "Backlog";
  return {
    id,
    team,
    priority,
    estimate: null,
    state: state as Ticket["state"],
    created_at: row.created_at ? new Date(row.created_at) : new Date(0),
    completed_at: row.completed_at ? new Date(row.completed_at) : null,
  };
}

function priorityFromCacheValue(v: number | string | undefined): Priority {
  if (typeof v === "string") {
    const lower = v.toLowerCase();
    if (lower === "urgent" || lower === "high" || lower === "medium" || lower === "low") {
      return lower;
    }
  }
  if (typeof v === "number") {
    if (v === 1) return "urgent";
    if (v === 2) return "high";
    if (v === 3) return "medium";
    if (v === 4) return "low";
  }
  return "medium";
}

function isEnoent(e: unknown): boolean {
  return (
    typeof e === "object" &&
    e !== null &&
    "code" in e &&
    (e as { code?: string }).code === "ENOENT"
  );
}
