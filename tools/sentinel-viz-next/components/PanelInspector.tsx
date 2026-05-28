"use client";

import { useEffect, useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { IconButton, Tooltip } from "@mui/material";
import CloseIcon from "@mui/icons-material/CloseRounded";

import type { Node, RecentEvent } from "../types/api";
import { fetchActivity, fetchSummary } from "../adapters/http";
import { indexActivity } from "../adapters/activity-cache";
import { awaitingKindLabel, categoryColor, categoryLabel, relTime, statusColor, statusLabel } from "../domain/format";
import { sentinelEventPhrase } from "./EventTicker";

export function friendlyTitle(node: Node): string {
  switch (node.type) {
    case "SentinelSession": {
      // Anonymous "session" was a frequent operator complaint — they
      // had to scan the body to identify which session was loaded.
      // Prefer the LLM-assigned name when available (set by the
      // backend's name-session endpoint and cached in node.data),
      // fall back to the short sid.
      const name =
        typeof node.data?.name === "string" && (node.data.name as string).length > 0
          ? (node.data.name as string)
          : null;
      const sid =
        typeof node.data?.session_id === "string"
          ? (node.data.session_id as string).slice(0, 8)
          : null;
      if (name && sid) return `${name} · s:${sid}`;
      if (name) return name;
      if (sid) return `session · s:${sid}`;
      return "session";
    }
    case "SentinelToolCall":
      return typeof node.data?.tool === "string" && (node.data.tool as string).length > 0
        ? `tool · ${node.data.tool}`
        : "tool call";
    case "SentinelHookInvocation":
      return typeof node.data?.hook === "string"
        ? `hook · ${node.data.hook as string}`
        : "hook";
    default:
      return node.type;
  }
}

const TC_TOOLS = new Set(["Bash", "Read", "Write", "Edit", "Grep", "Glob", "NotebookEdit", "MultiEdit"]);
const PLANNING_TOOLS = new Set(["TaskCreate", "TaskUpdate", "TaskList", "TaskGet", "TaskStop", "TaskOutput", "WebFetch", "WebSearch", "Plan", "ExitPlanMode", "EnterPlanMode"]);
const COMMUNICATION_TOOLS = new Set(["Agent", "AskUserQuestion", "Stop", "ToolSearch"]);

function segmentStyle(
  kind: string,
  hadError: boolean,
  tools: string[],
): { border: string; label: string; bg: string } {
  if (hadError) return { border: "#D71921", label: "#D71921", bg: "#1a0606" };
  if (kind === "user_input") return { border: "#5B9BF6", label: "#5B9BF6", bg: "#0A1525" };
  // Assistant turn — color by the dominant tool category within the turn.
  if (tools.length > 0) {
    const t = tools[0];
    if (TC_TOOLS.has(t)) return { border: "#4A9E5C", label: "#4A9E5C", bg: "#0A1A0A" };
    if (PLANNING_TOOLS.has(t)) return { border: "#D4A843", label: "#D4A843", bg: "#1A1408" };
    if (COMMUNICATION_TOOLS.has(t)) return { border: "#bc8cff", label: "#bc8cff", bg: "#0F0818" };
  }
  // Text-only assistant turn (no tool use).
  return { border: "#bc8cff", label: "#bc8cff", bg: "#0F0818" };
}

interface Props {
  node: Node | null;
  /** Anchor timestamp from the ticker click. When set, activity is
   * fetched as a ±60s window around it instead of just the tail. */
  anchorTs?: string | null;
  /** Recent events from the current graph snapshot. Used by the
   * codex/non-claude fallback timeline view when transcript JSONLs
   * aren't available. */
  events?: RecentEvent[];
  onClose: () => void;
}

export function PanelInspector({ node, anchorTs, events, onClose }: Props) {
  const sessionId =
    node?.type === "SentinelSession"
      ? (node.data?.session_id as string | undefined)
      : (node?.data?.session_id as string | undefined);

  // /api/activity and /api/summary both read claude transcript JSONLs
  // from ~/.claude/projects/. Non-claude harnesses don't have those,
  // so gate the fetch by harness to avoid stuck "loading…" /
  // "generating…" placeholders. Codex sessions get a different
  // fallback panel (CodexEventTimeline below) sourced from the
  // graph's event stream instead.
  const harness = (node?.data?.source_harness as string | undefined) ?? null;
  const harnessSupportsTranscript = !harness || harness === "claude";

  const activityQ = useQuery({
    queryKey: ["activity", sessionId, anchorTs ?? null],
    queryFn: ({ signal }) =>
      fetchActivity(
        sessionId!,
        anchorTs ? { limit: 30, atTs: anchorTs, windowSecs: 60 } : { limit: 10 },
        signal,
      ),
    enabled: !!sessionId && harnessSupportsTranscript,
    staleTime: 5_000,
  });
  const fallbackRows = useMemo(
    () => (sessionId ? buildHarnessTimelineRows(events ?? [], sessionId).slice(0, 12) : []),
    [events, sessionId],
  );

  // Feed the activity-cache for the EventTicker label join.
  useEffect(() => {
    if (sessionId && activityQ.data) indexActivity(sessionId, activityQ.data);
  }, [sessionId, activityQ.data]);

  // "last activity" relative string. The React Compiler memoizes this
  // function body, so a plain const (not a hand-written useMemo, which
  // the compiler rejects when its deps are narrower than inferred) is
  // the idiomatic form here. Date.now() is flagged impure in render;
  // it's intentional — the age is relative to wall-clock-now and the
  // value re-derives on the next render tick, which is the behaviour we
  // want for an "X ago" readout.
  // eslint-disable-next-line react-hooks/purity
  const nowMs = Date.now();
  const lastActivityLabel =
    node?.last_activity_age_s != null
      ? relTime(new Date(nowMs - node.last_activity_age_s * 1000).toISOString())
      : null;

  const summaryKind: "wait" | "card" = node?.session_status === "awaiting_user" ? "wait" : "card";
  const summaryQ = useQuery({
    queryKey: ["summary", sessionId, anchorTs ?? null, summaryKind],
    queryFn: ({ signal }) =>
      fetchSummary(sessionId!, { kind: summaryKind, atTs: anchorTs ?? undefined }, signal),
    enabled: !!sessionId && harnessSupportsTranscript,
    staleTime: 60_000,
  });

  if (!node) {
    return (
      <section
        data-testid="panel-inspector"
        className="w-full md:w-[360px] flex-1 md:flex-none min-h-0 max-h-screen md:max-h-none border-t md:border-t-0 md:border-l border-[#222] bg-[#111] text-[#E8E8E8] p-4 text-xs font-mono"
      >
        <p className="text-[#999]">click a node or ticker row to inspect</p>
      </section>
    );
  }

  return (
    <section
      data-testid="panel-inspector"
      className="w-full md:w-[360px] border-t md:border-t-0 md:border-l border-[#222] bg-[#111] text-[#E8E8E8] p-4 text-xs font-mono overflow-y-auto"
    >
      <header className="flex items-baseline justify-between mb-3">
        <h3 className="text-[#E8E8E8] text-sm">
          <span className="text-[#5B9BF6]">{friendlyTitle(node)}</span>
        </h3>
        <Tooltip title="close inspector">
          <IconButton
            aria-label="close inspector"
            onClick={onClose}
            size="small"
            sx={{ mr: -1, my: -0.5 }}
          >
            <CloseIcon fontSize="small" />
          </IconButton>
        </Tooltip>
      </header>
      <div className="space-y-1 text-[11px]">
        {node.category ? (
          <div className="flex justify-between">
            <span>category</span>
            <span style={{ color: categoryColor(node.category) }}>
              {categoryLabel(node.category)}
            </span>
          </div>
        ) : null}
        {node.session_status ? (
          <div className="flex justify-between">
            <span>status</span>
            <span
              style={{ color: statusColor(node.session_status) }}
              title={node.session_status}
            >
              {statusLabel(node.session_status)}
            </span>
          </div>
        ) : null}
        {lastActivityLabel != null ? (
          <Row k="last activity" v={lastActivityLabel} />
        ) : null}
        {typeof node.data?.session_id === "string" ? (
          <Row k="session" v={(node.data.session_id as string).slice(0, 12) + "…"} />
        ) : null}
        {typeof node.data?.n_hooks === "number" ? (
          <Row k="hooks fired" v={String(node.data.n_hooks)} />
        ) : null}
        {typeof node.data?.outcomes === "object" && node.data.outcomes ? (
          <Row k="outcomes" v={Object.entries(node.data.outcomes as Record<string, number>).map(([k, v]) => `${v} ${k}`).join(", ")} />
        ) : null}
        <Row k="id" v={node.id.replace(/^Sentinel/, "")} />
      </div>

      {node.awaiting_question && shouldShowRawAwaiting(summaryKind, summaryQ.data?.text ?? null) ? (
        <div
          data-testid="raw-awaiting-block"
          className="mt-4 p-2 bg-[#000] border border-[#222] rounded"
        >
          <div
            className="text-[10px] uppercase text-[#bc8cff] mb-1"
            title={node.awaiting_kind ?? undefined}
          >
            awaiting user · {awaitingKindLabel(node.awaiting_kind)}
          </div>
          <div className="text-[11px] whitespace-pre-wrap">{node.awaiting_question}</div>
        </div>
      ) : null}

      <details className="mt-4">
        <summary className="cursor-pointer text-[#999] text-[10px] uppercase tracking-wider">data</summary>
        <pre className="mt-2 text-[10px] bg-[#000] p-2 rounded border border-[#222] overflow-x-auto">
          {JSON.stringify(node.data, null, 2)}
        </pre>
      </details>

      {sessionId && !harnessSupportsTranscript ? (
        <CodexEventTimeline
          sessionId={sessionId}
          harness={harness}
          events={events ?? []}
        />
      ) : null}

      {sessionId && harnessSupportsTranscript ? (
        <div className="mt-4 border-t border-[#222] pt-3">
          <SummaryCard
            kind={summaryKind}
            text={summaryQ.data?.text ?? null}
            source={summaryQ.data?.source ?? null}
            loading={summaryQ.isPending}
            error={summaryQ.error ? "summary unavailable" : null}
          />
          <h4 className="text-[10px] uppercase tracking-wider text-[#999] mb-2 flex justify-between mt-4">
            <span>{anchorTs ? "activity ± 60s" : "recent activity"}</span>
            {anchorTs ? <span className="text-[#5B9BF6]">@ {anchorTs.slice(11, 19)}</span> : null}
          </h4>
          {activityQ.data?.segments.length ? (
            <ul className="space-y-2" data-testid="activity-segments">
              {activityQ.data.segments.slice(-12).reverse().map((s, i) => {
                const sty = segmentStyle(s.kind, !!s.had_error, s.tools ?? []);
                const hasText = !!s.text && s.text.trim().length > 0;
                // user_input segments only populate `preview`, not `text`.
                // Render whichever has content.
                const bodyText = hasText ? s.text! : (s.preview?.trim() ?? "");
                const hasBody = bodyText.length > 0;
                const calls = s.tool_calls ?? [];
                return (
                  <li
                    key={`${s.ts}-${i}`}
                    className="p-2 rounded border-l-2"
                    style={{ borderLeftColor: sty.border, backgroundColor: sty.bg }}
                  >
                    <div className="flex justify-between items-baseline text-[10px] mb-1 gap-2">
                      <span className="font-bold truncate" style={{ color: sty.label }}>
                        {s.kind === "user_input" ? "user input" : s.label}
                      </span>
                      <span className="text-[#999] whitespace-nowrap">{relTime(s.ts)}</span>
                    </div>
                    {hasBody ? (
                      <div className="text-[10px] text-[#E8E8E8] opacity-90 whitespace-pre-wrap break-words mb-1">
                        {bodyText}
                      </div>
                    ) : null}
                    {calls.length > 0 ? (
                      <ul className="space-y-1 mt-1">
                        {calls.map((tc, j) => (
                          <li
                            key={`${tc.id || j}`}
                            className="text-[10px] pl-2 border-l border-[#222]"
                          >
                            <div className="flex justify-between gap-2">
                              <span className="font-mono text-[#4A9E5C]">{tc.tool}</span>
                              {tc.error ? (
                                <span className="text-[#D71921]">error</span>
                              ) : tc.result_preview ? (
                                <span className="text-[#999]">ok</span>
                              ) : (
                                <span className="text-[#999]">pending</span>
                              )}
                            </div>
                            <div className="text-[10px] text-[#E8E8E8] opacity-90 whitespace-pre-wrap break-words">
                              {tc.summary && tc.summary !== `(${tc.tool})` ? tc.summary : (
                                <span className="opacity-50">(no args)</span>
                              )}
                            </div>
                            {tc.result_preview ? (
                              <div
                                className="text-[10px] mt-0.5 opacity-70 italic"
                                style={{ color: tc.error ? "#D71921" : "#999" }}
                              >
                                → {tc.result_preview}
                              </div>
                            ) : null}
                          </li>
                        ))}
                      </ul>
                    ) : !hasBody ? (
                      <div className="text-[10px] text-[#999] italic">
                        (empty event — likely Stop or hook artefact)
                      </div>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          ) : fallbackRows.length > 0 ? (
            <GraphEventActivitySegments rows={fallbackRows} />
          ) : activityQ.isPending ? (
            <p className="text-[#999]">loading…</p>
          ) : activityQ.error ? (
            <p className="text-[#D71921]">activity error</p>
          ) : (
            <p className="text-[#999]">no segments in window</p>
          )}
        </div>
      ) : null}
    </section>
  );
}

/// When the SummaryCard's "what it's waiting on" variant has rendered
/// an LLM-generated rollup of the same question, the raw awaiting
/// callout is a duplicate. Hide it then. Show it when the summary is
/// disabled, errored, or hasn't loaded text yet — that way the
/// operator never loses sight of the raw question.
export function shouldShowRawAwaiting(
  summaryKind: "card" | "wait",
  summaryText: string | null,
): boolean {
  if (summaryKind !== "wait") return true;
  return !summaryText || summaryText.trim().length === 0;
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-[#999]">{k}</span>
      <span className="text-[#5B9BF6] truncate">{v}</span>
    </div>
  );
}

function GraphEventActivitySegments({ rows }: { rows: HarnessTimelineRow[] }) {
  return (
    <ul className="space-y-2" data-testid="activity-segments">
      {rows.map((row, i) => {
        const accent = outcomeAccent(row.outcome);
        return (
          <li
            key={`${row.seq}-${i}`}
            className="p-2 rounded border-l-2"
            style={{ borderLeftColor: accent, backgroundColor: "#000" }}
          >
            <div className="flex justify-between items-baseline text-[10px] mb-1 gap-2">
              <span className="font-bold truncate" style={{ color: accent }}>
                {row.count > 1 ? `×${row.count} ` : ""}{row.label}
              </span>
              <span className="text-[#999] whitespace-nowrap">{relTime(row.ts)}</span>
            </div>
            {row.detail || row.outcome ? (
              <div className="flex justify-between gap-2 text-[10px] text-[#999]">
                <span className="truncate">{row.detail}</span>
                {row.outcome ? <span>{row.outcome}</span> : null}
              </div>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
}

interface SummaryCardProps {
  kind: "card" | "wait";
  text: string | null;
  source: string | null;
  loading: boolean;
  error: string | null;
}

/// Event-timeline panel for non-claude harnesses (codex). Built from
/// graph.events filtered to the current session. The transcript-based
/// activity panel can't fire here because find_transcript() only
/// scans claude paths — but the bridge has been producing hook
/// records for codex sessions all along via the shim, so we already
/// have everything we need from the events stream.
///
/// Exported for testing. Renders into the inspector body when the
/// surrounding session is non-claude AND has events in the current
/// snapshot.
export function CodexEventTimeline({
  sessionId,
  harness,
  events,
}: {
  sessionId: string;
  harness: string | null;
  events: RecentEvent[];
}) {
  const rows = buildHarnessTimelineRows(events, sessionId).slice(0, 20);

  return (
    <div className="mt-4 border-t border-[#222] pt-3">
      <h4 className="text-[10px] uppercase tracking-wider text-[#999] mb-2 flex justify-between">
        <span>{harness ?? "non-claude"} timeline</span>
        <span className="text-[#666]">last {rows.length}</span>
      </h4>
      {rows.length === 0 ? (
        <p className="text-[10px] text-[#999] italic">
          no events for this session in the current snapshot
        </p>
      ) : (
        <ul className="space-y-1" data-testid="codex-event-timeline">
          {rows.map((row, i) => {
            const accent = outcomeAccent(row.outcome);
            return (
              <li
                key={`${row.seq}-${i}`}
                className="p-1.5 rounded border-l-2 text-[10px]"
                style={{ borderLeftColor: accent, backgroundColor: "#000" }}
              >
                <div className="flex justify-between gap-2 mb-0.5">
                  <span className="font-bold" style={{ color: accent }}>
                    {row.count > 1 ? `×${row.count} ` : ""}{row.label}
                  </span>
                  <span className="text-[#999] whitespace-nowrap">{relTime(row.ts)}</span>
                </div>
                {row.detail || row.outcome ? (
                  <div className="flex justify-between gap-2 text-[#999]">
                    <span className="truncate">{row.detail}</span>
                    {row.outcome ? <span>{row.outcome}</span> : null}
                  </div>
                ) : null}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

interface HarnessTimelineRow {
  seq: number;
  ts: string;
  label: string;
  detail: string;
  outcome: string;
  count: number;
}

function buildHarnessTimelineRows(events: RecentEvent[], sessionId: string): HarnessTimelineRow[] {
  const rows: HarnessTimelineRow[] = [];
  const newest = events
    .filter((e) => e.payload.session_id === sessionId)
    .slice()
    .reverse();
  for (const e of newest) {
    const tool = typeof e.payload.tool === "string" ? (e.payload.tool as string) : "";
    const hook = typeof e.payload.hook === "string" ? (e.payload.hook as string) : "";
    const sentinelEvent = typeof e.payload.sentinel_event === "string"
      ? (e.payload.sentinel_event as string)
      : typeof e.payload.event === "string"
        ? (e.payload.event as string)
        : e.type;
    const outcome = typeof e.payload.outcome === "string"
      ? (e.payload.outcome as string)
      : "";
    if (isLowSignalHarnessTimelineEvent(sentinelEvent, tool, hook, outcome)) continue;
    const label = tool || friendlyTimelineEvent(sentinelEvent, hook);
    const rawDetail = timelineDetail(sentinelEvent, hook);
    // Drop the detail when it just restates the label — a tool-less
    // lifecycle row (label "notified") shouldn't carry a duplicate
    // "notified" sub-line, and we never want the raw event name to
    // leak there either.
    const detail = rawDetail === label ? "" : rawDetail;
    const sig = `${label}\t${detail}\t${outcome}`;
    const prev = rows[rows.length - 1];
    if (prev && `${prev.label}\t${prev.detail}\t${prev.outcome}` === sig) {
      prev.count += 1;
      prev.seq = Math.max(prev.seq, e.seq);
      prev.ts = e.ts;
      continue;
    }
    rows.push({
      seq: e.seq,
      ts: e.ts,
      label,
      detail,
      outcome,
      count: 1,
    });
  }
  return rows;
}

function isLowSignalHarnessTimelineEvent(
  sentinelEvent: string,
  tool: string,
  hook: string,
  outcome: string,
): boolean {
  if (outcome === "deny" || outcome === "block" || outcome === "force_stop" || outcome === "inject") {
    return false;
  }
  const h = hook.toLowerCase();
  const t = tool.toLowerCase();
  if (h === "codex_shim_tool_result") return true;
  if (t === "write_stdin" || t === "codex_shim_tool_result") return true;
  return sentinelEvent === "PostToolUse" && !tool && h.includes("tool_result");
}

function friendlyTimelineEvent(sentinelEvent: string, hook: string): string {
  if (sentinelEvent === "UserPromptSubmit") return "user prompt";
  if (sentinelEvent === "SessionStart") return "session started";
  if (sentinelEvent === "Stop") return "session stopped";
  // A named hook is more specific than the lifecycle event, so prefer
  // it. Otherwise route through the same translation the Claude ticker
  // uses so a no-tool/no-hook codex event reads "notified" /
  // "compacting" / "about to run" instead of leaking the raw
  // `Notification` / `PreCompact` / `PreToolUse` lifecycle name.
  return hook || sentinelEventPhrase(sentinelEvent);
}

function timelineDetail(sentinelEvent: string, hook: string): string {
  // Translate the lifecycle event through the same phrase table the
  // Claude ticker uses so the detail line never shows raw jargon
  // ("PreToolUse" → "about to run"). When a hook is present, keep it
  // appended — the hook name is operator-meaningful and specific.
  const phrase = sentinelEventPhrase(sentinelEvent);
  if (hook && hook !== "codex_shim") return `${phrase} · ${hook}`;
  return phrase;
}

/// Mirror of EventTicker's outcome categorisation, narrowed for the
/// codex timeline. Intervention outcomes pop in red; routine allows
/// stay muted; everything else is the default text accent.
function outcomeAccent(outcome: string): string {
  if (outcome === "deny" || outcome === "block" || outcome === "force_stop") return "#D71921";
  if (outcome === "inject") return "#D4A843";
  if (outcome === "allow" || outcome === "") return "#999999";
  return "#5B9BF6";
}

function SummaryCard({ kind, text, source, loading, error }: SummaryCardProps) {
  const label = kind === "wait" ? "what it's waiting on" : "ai summary";
  const accent = kind === "wait" ? "#D4A843" : "#5B9BF6";
  // Hide the card entirely when naming is disabled — no value in
  // showing an empty stub. Show it as soon as we have ANY signal.
  if (!loading && !error && !text && source === "disabled") return null;
  return (
    <div
      data-testid="ai-summary"
      className="p-2 rounded mb-3 border-l-2"
      style={{ borderLeftColor: accent, backgroundColor: "#000" }}
    >
      <div className="flex justify-between items-baseline text-[10px] uppercase tracking-wider mb-1">
        <span style={{ color: accent }}>{label}</span>
        {source ? <span className="text-[#666]">{source}</span> : null}
      </div>
      {loading ? (
        <div className="text-[10px] text-[#999] italic">generating…</div>
      ) : error ? (
        <div className="text-[10px] text-[#D71921]">{error}</div>
      ) : text ? (
        <div className="text-[11px] text-[#E8E8E8] whitespace-pre-wrap leading-relaxed">{text}</div>
      ) : (
        <div className="text-[10px] text-[#999] italic">
          (no summary — naming model disabled or activity empty)
        </div>
      )}
    </div>
  );
}
