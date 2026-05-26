"use client";

import { useQuery } from "@tanstack/react-query";

import { bucketsToSparkline } from "../domain/session-strips";
import type { SessionStripData } from "../domain/session-strips";
import { fetchSummary } from "../adapters/http";
import { categoryColor, categoryLabel, statusColor } from "../domain/format";

interface Props {
  data: SessionStripData;
  /** Width in pixels of the sparkline column. The bar string is
   *  fixed-width mono so this controls how many bars are visible
   *  in the available row width — we don't truncate; CSS overflow
   *  handles narrow panes. */
  selected: boolean;
  onSelect: () => void;
}

/// One row per active session. Variant B from the design review:
/// per-tool-category sparkline rows so the operator sees the
/// rhythm of WHAT KIND of work each session is doing across the
/// window.
export function SessionStrip({ data, selected, onSelect }: Props) {
  const isStuck = !!data.stuck;
  const statusText = data.status ?? "—";

  // P3-32: pull the narrative AI summary for this session so the
  // empty horizontal space on each strip carries a 1-2 line plain-
  // english overview of what's happening. Cached 60s server-side
  // (see summary.rs). The wait-kind variant fires instead when
  // the session is awaiting_user — operator sees "what it's
  // waiting on" in that case, which is the higher-signal copy.
  const summaryKind = isStuck || data.status === "awaiting_user" ? "wait" : "narrative";
  // AI summary endpoint reads claude transcript JSONLs from
  // ~/.claude/projects/. Non-claude harnesses (codex / opencode /
  // qwen / gemini) don't have those files, so the request would
  // 404 or return text=null forever and the strip would render a
  // stuck "ai · generating summary…" placeholder. Gate the query
  // by harness so we never hit the endpoint when there's nothing
  // to fetch.
  const harnessSupportsSummary =
    !data.sourceHarness || data.sourceHarness === "claude";
  const summaryQ = useQuery({
    queryKey: ["strip-summary", data.sessionId, summaryKind],
    queryFn: ({ signal }) => fetchSummary(data.sessionId, { kind: summaryKind }, signal),
    enabled: !!data.sessionId && harnessSupportsSummary,
    staleTime: 60_000,
    refetchInterval: 60_000,
  });
  const summaryText = summaryQ.data?.text?.trim();
  const summaryAvailable = !!summaryText && summaryText.length > 0;
  const summaryDisabled = summaryQ.data?.source === "disabled";
  // The /api/summary endpoint can resolve with `text: null` even
  // when the LLM is configured — typically when the upstream call
  // errored or the input window had no useful activity. The pre-
  // P3-33 UI rendered NOTHING in that case, which looked like the
  // AI feature was silently broken. Now we surface an explicit
  // empty-state message so the operator can tell "no rollup
  // available right now" from "the feature is broken".
  const summaryFailedSilently =
    !!summaryQ.data && !summaryAvailable && !summaryDisabled && !summaryQ.isPending;
  return (
    <li
      data-testid="session-strip"
      data-session-id={data.sessionId}
      data-status={statusText}
      data-stuck={isStuck ? "true" : undefined}
      data-selected={selected ? "true" : undefined}
      onClick={onSelect}
      className={`px-3 py-2 border-b border-[#222] cursor-pointer flex gap-3 ${
        selected ? "bg-[#5B9BF622]" : "hover:bg-[#5B9BF614]"
      }`}
    >
      {/* Session-color tab — 4px wide, full row height, matches
          the ticker / inspector use of session colour. */}
      <span
        className="shrink-0 self-stretch rounded-sm"
        style={{
          width: "4px",
          backgroundColor: data.color,
        }}
        title={`session ${data.shortSid}`}
      />
      <div className="flex-1 min-w-0">
        {/* Header line: status dot, name, status badge,
            last-activity. Compact. */}
        <div className="flex items-baseline gap-2 text-[11px]">
          <span
            className="inline-block w-2 h-2 rounded-full shrink-0"
            style={{ backgroundColor: statusColor(data.status) }}
            title={statusText}
          />
          <span
            className="font-bold truncate"
            style={{ color: data.color }}
            data-testid="session-strip-name"
          >
            {data.displayName}
          </span>
          <span className="text-[#999] text-[10px]">{statusText}</span>
          {data.sourceHarness ? (
            <span
              data-testid="session-strip-harness"
              data-harness={data.sourceHarness}
              className="text-[9px] uppercase tracking-wider px-1.5 py-0.5 rounded border"
              style={{
                color: harnessColor(data.sourceHarness),
                borderColor: harnessColor(data.sourceHarness),
                backgroundColor: harnessColor(data.sourceHarness) + "1A",
              }}
              title={`harness: ${data.sourceHarness}`}
            >
              {data.sourceHarness}
            </span>
          ) : null}
          <span className="ml-auto text-[#999] text-[10px] whitespace-nowrap">
            {formatAge(data.lastActivityAgeS)} · {data.totalEvents} ev
          </span>
        </div>

        {/* Per-category sparklines. One row per category that saw
            activity in the window. Bars are unicode block chars
            normalised against the session's own peak so a quiet
            "edit" still shows its rhythm even when "bash" is
            dominant. */}
        <ul className="mt-1 space-y-0 font-mono text-[10px] leading-tight">
          {data.rows.map((row) => (
            <li
              key={row.category}
              data-testid="session-strip-category"
              data-category={row.category}
              className="flex items-baseline gap-2"
            >
              <span
                className="shrink-0 w-12 truncate uppercase tracking-wider text-[9px]"
                style={{ color: categoryColor(row.category) }}
                title={categoryLabel(row.category)}
              >
                {categoryLabel(row.category)}
              </span>
              <span
                className="flex-1 truncate text-[#E8E8E8]"
                title={`${row.total} ${categoryLabel(row.category)} events, peak ${row.peak}/min`}
                style={{ letterSpacing: "-0.04em" }}
              >
                {bucketsToSparkline(row.counts, data.peakPerMin || row.peak)}
              </span>
              <span className="shrink-0 text-[9px] text-[#999] tabular-nums">
                {row.total}
              </span>
            </li>
          ))}
        </ul>

        {/* AI summary line (P3-32). Fills the wide-screen empty
            real estate with a 1-2 sentence narrative pulled from
            the LLM. Only renders when we have actual text; the
            stuck banner below takes precedence when both apply. */}
        {summaryAvailable && !isStuck ? (
          <div
            data-testid="session-strip-ai-summary"
            className="mt-1 text-[10px] text-[#999] leading-tight line-clamp-2"
            title={summaryText}
          >
            <span className="text-[#5B9BF6] mr-1 uppercase tracking-wider text-[9px]">
              ai
            </span>
            {summaryText}
          </div>
        ) : null}
        {/* When the summary is loading and we don't yet have text,
            keep the layout calm — show a tiny ghost placeholder so
            the strip doesn't jump when the text arrives. Suppress
            for non-claude harnesses where the query is disabled by
            design (TanStack Query reports pending=true forever when
            `enabled: false`, otherwise we'd render this placeholder
            permanently on every codex/opencode/qwen/gemini strip). */}
        {harnessSupportsSummary &&
          !summaryAvailable &&
          !summaryDisabled &&
          !isStuck &&
          summaryQ.isPending ? (
          <div className="mt-1 text-[10px] text-[#666] italic leading-tight">
            ai · generating summary…
          </div>
        ) : null}
        {/* P3-33: explicit empty state when the summary endpoint
            resolved with text=null. Without this, the operator
            sees nothing where an AI summary should be and assumes
            the feature is broken. */}
        {summaryFailedSilently && !isStuck ? (
          <div
            data-testid="session-strip-ai-unavailable"
            className="mt-1 text-[10px] text-[#666] italic leading-tight"
            title={`Source: ${summaryQ.data?.source ?? "unknown"}`}
          >
            ai · no rollup available
            {summaryQ.data?.source ? (
              <span className="text-[#222] ml-1">({summaryQ.data.source})</span>
            ) : null}
          </div>
        ) : null}
        {/* Stuck callout — P3-34: restored the v1 viz's red-
            bordered prominent box that includes BOTH the AI
            "what it's waiting on" rollup AND the raw question.
            Operator screenshot review: "it's completely unclear
            what is stuck here, we used to get a red or yellow
            highlighted box with the AI summary of what needed to
            happen to unblock the stuckness". The one-line banner
            was too easy to miss.
            Structure:
              ⚠ STUCK 57m · AskUserQuestion
              ai  Operator should acknowledge burst status and
                  confirm idling…   ← LLM rollup of what's needed
              raw  Status reported. Burst healthy, no… ← raw text
        */}
        {data.stuck ? (
          <div
            data-testid="session-strip-stuck"
            className="mt-1.5 rounded border border-[#D71921] bg-[#1a060633] p-2 space-y-1"
          >
            <div className="text-[10px] font-bold text-[#D71921] flex items-baseline gap-1.5 flex-wrap">
              <span>⚠ STUCK</span>
              <span className="text-[#FFA198]">{formatStuckAge(data.stuck.ageSecs)}</span>
              <span className="text-[#FFA198]">·</span>
              <span className="text-[#FFA198]">{data.stuck.kind ?? "awaiting"}</span>
            </div>
            {/* AI "what needs to happen" rollup. Only renders
                when we have actual text — falls back to a hint
                otherwise so the operator can tell pending from
                disabled. */}
            {summaryAvailable ? (
              <div
                data-testid="session-strip-stuck-ai"
                className="text-[10px] text-[#FFA198] leading-snug"
                title={summaryText}
              >
                <span className="uppercase tracking-wider text-[9px] text-[#D71921] mr-1">
                  ai
                </span>
                {summaryText}
              </div>
            ) : summaryQ.isPending ? (
              <div className="text-[10px] text-[#5a1010] italic leading-snug">
                <span className="uppercase tracking-wider text-[9px] mr-1">ai</span>
                generating "what's needed"…
              </div>
            ) : null}
            {/* Raw question — always shown when present.
                Operators sometimes need to see the literal text the
                agent asked, not just the LLM rollup. Multi-line so
                long questions don't get truncated. */}
            {data.stuck.question ? (
              <div className="text-[10px] text-[#E8E8E8] leading-snug">
                <span className="uppercase tracking-wider text-[9px] text-[#999] mr-1">
                  raw
                </span>
                {data.stuck.question}
              </div>
            ) : null}
          </div>
        ) : null}
      </div>
    </li>
  );
}

function formatAge(secs: number | null): string {
  if (secs == null) return "—";
  if (secs < 5) return "now";
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h`;
  return `${Math.round(secs / 86400)}d`;
}

function formatStuckAge(secs: number): string {
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  return `${Math.round(secs / 3600)}h`;
}

/// Per-harness identity color. Distinct from session palette so the
/// operator can scan-filter by harness independent of session colour.
/// claude=info-blue (the canonical home harness), codex=warning-amber
/// (OpenAI), opencode=purple, qwen=teal (Alibaba), gemini=success-green
/// (Google). Unknown harnesses fall through to text-secondary.
function harnessColor(h: string): string {
  switch (h) {
    case "claude":   return "#5B9BF6";
    case "codex":    return "#D4A843";
    case "opencode": return "#bc8cff";
    case "qwen":     return "#4FB3B3";
    case "gemini":   return "#4A9E5C";
    default:         return "#999999";
  }
}
