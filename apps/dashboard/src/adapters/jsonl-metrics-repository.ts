// SENTINEL-24 — JSONL metrics repository adapter.
//
// Reads the JSONL files sentinel collectors append under
// `~/.claude/sentinel/metrics/`. Missing files degrade to empty arrays;
// malformed lines are skipped (not thrown) so a single corrupt write
// doesn't tank an entire dashboard render.
//
// Source schemas (canonical references):
//   * cycle-time.jsonl  → crates/sentinel-application/src/cycle_time.rs (SEN-1)
//   * deploys.jsonl     → crates/sentinel-application/src/deploy_freq.rs (SEN-9)
//   * tokens-per-ticket.jsonl → crates/sentinel-application/src/tokens.rs (SEN-7)
//   * roi.jsonl         → crates/sentinel-application/src/roi.rs (SEN-15)

import { promises as fs } from "node:fs";
import path from "node:path";

import {
  STAGES,
  type Stage,
  type StageTransition,
  makeHours,
  makeTicketIdentifier,
  type TicketIdentifier,
} from "../domain";
import type {
  DeployEvent,
  Incident,
  MetricsRepository,
  TimeRange,
  TokenSession,
} from "../ports";

const STAGE_SET: ReadonlySet<string> = new Set(STAGES);
const TICKET_ID_RE = /^[A-Z]+-\d+$/;
const MS_PER_HOUR = 1000 * 60 * 60;

/** Raw row in cycle-time.jsonl. */
interface CycleTimeRow {
  readonly issue_id: string;
  readonly team?: string;
  readonly from_state?: string | null;
  readonly to_state: string;
  readonly timestamp: string;
}

/** Raw row in deploys.jsonl. */
interface DeployRow {
  readonly timestamp: string;
  readonly repo: string;
  readonly env: string;
  readonly commit: string;
  readonly duration_s?: number;
}

/** Raw row in tokens-per-ticket.jsonl. */
interface TokensPerTicketRow {
  readonly ticket: string;
  readonly sessions: number;
  readonly total_input: number;
  readonly cache_read: number;
  readonly cache_creation: number;
  readonly output: number;
  readonly cost_usd: number;
  readonly models?: Record<string, number>;
}

export class JsonlMetricsRepository implements MetricsRepository {
  /** Absolute path to `~/.claude/sentinel/metrics/`. */
  private readonly metricsDir: string;

  /**
   * @param homeDir absolute home directory (typically `os.homedir()`). Pass
   * a temp dir from tests.
   */
  constructor(homeDir: string) {
    this.metricsDir = path.join(homeDir, ".claude", "sentinel", "metrics");
  }

  async readCycleTimeEvents(window: TimeRange): Promise<StageTransition[]> {
    const rows = await this.readJsonl<CycleTimeRow>("cycle-time.jsonl");
    // Group by issue_id, sort by timestamp, derive `hours` from adjacent ts diff.
    const byIssue = new Map<string, CycleTimeRow[]>();
    for (const row of rows) {
      if (typeof row.issue_id !== "string") continue;
      const list = byIssue.get(row.issue_id) ?? [];
      list.push(row);
      byIssue.set(row.issue_id, list);
    }
    const out: StageTransition[] = [];
    for (const list of byIssue.values()) {
      list.sort((a, b) => a.timestamp.localeCompare(b.timestamp));
      let prevTsMs: number | null = null;
      for (const row of list) {
        const ts = new Date(row.timestamp);
        if (Number.isNaN(ts.getTime())) {
          prevTsMs = null;
          continue;
        }
        const from = row.from_state ?? null;
        if (!from || !STAGE_SET.has(from) || !STAGE_SET.has(row.to_state)) {
          prevTsMs = ts.getTime();
          continue;
        }
        const diffMs = prevTsMs === null ? 0 : Math.max(0, ts.getTime() - prevTsMs);
        out.push({
          from: from as Stage,
          to: row.to_state as Stage,
          ts,
          hours: makeHours(diffMs / MS_PER_HOUR),
        });
        prevTsMs = ts.getTime();
      }
    }
    return out.filter((t) => inWindow(t.ts, window));
  }

  async readDeploys(window: TimeRange): Promise<DeployEvent[]> {
    const rows = await this.readJsonl<DeployRow>("deploys.jsonl");
    const out: DeployEvent[] = [];
    for (const row of rows) {
      const ts = new Date(row.timestamp);
      if (Number.isNaN(ts.getTime())) continue;
      if (!inWindow(ts, window)) continue;
      out.push({
        timestamp: ts,
        repo: row.repo,
        env: row.env,
        commit: row.commit,
        durationS: row.duration_s ?? 0,
      });
    }
    return out;
  }

  async readTokenUsage(_window: TimeRange): Promise<TokenSession[]> {
    // tokens-per-ticket.jsonl is aggregate-by-ticket, no per-row timestamp,
    // so the window argument is ignored at this layer. Callers can filter
    // downstream when richer per-session data lands.
    const rows = await this.readJsonl<TokensPerTicketRow>("tokens-per-ticket.jsonl");
    const out: TokenSession[] = [];
    for (const row of rows) {
      if (typeof row.ticket !== "string") continue;
      let ticketId: TicketIdentifier | undefined;
      if (TICKET_ID_RE.test(row.ticket)) {
        try {
          ticketId = makeTicketIdentifier(row.ticket);
        } catch {
          ticketId = undefined;
        }
      }
      // Pick the model with the highest session count, or "<unknown>".
      let model = "<unknown>";
      if (row.models) {
        let best = -1;
        for (const [name, n] of Object.entries(row.models)) {
          if (typeof n === "number" && n > best) {
            best = n;
            model = name;
          }
        }
      }
      out.push({
        ...(ticketId ? { ticketId } : {}),
        sessionId: `agg:${row.ticket}`,
        totalInput: row.total_input ?? 0,
        cacheRead: row.cache_read ?? 0,
        cacheCreation: row.cache_creation ?? 0,
        output: row.output ?? 0,
        costUsd: row.cost_usd ?? 0,
        model,
      });
    }
    return out;
  }

  async readIncidents(_window: TimeRange): Promise<Incident[]> {
    // TODO(SEN-11): wire to incidents.jsonl once the change-failure-rate
    // collector lands. For now no source file exists — return empty so the
    // dashboard's CFR/MTTR panels render a "no data" state cleanly.
    return [];
  }

  /** Read every line of a JSONL file in `metricsDir`. Missing file → []. */
  private async readJsonl<T>(filename: string): Promise<T[]> {
    const filePath = path.join(this.metricsDir, filename);
    let raw: string;
    try {
      raw = await fs.readFile(filePath, "utf8");
    } catch (e) {
      if (isEnoent(e)) return [];
      throw e;
    }
    const out: T[] = [];
    for (const line of raw.split(/\r?\n/)) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        out.push(JSON.parse(trimmed) as T);
      } catch {
        // Skip malformed row — collector may have been mid-write.
      }
    }
    return out;
  }
}

function inWindow(ts: Date, window: TimeRange): boolean {
  return ts.getTime() >= window.start.getTime() && ts.getTime() < window.end.getTime();
}

function isEnoent(e: unknown): boolean {
  return (
    typeof e === "object" &&
    e !== null &&
    "code" in e &&
    (e as { code?: string }).code === "ENOENT"
  );
}
