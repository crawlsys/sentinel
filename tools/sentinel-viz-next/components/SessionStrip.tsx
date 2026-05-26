"use client";

import { useQuery } from "@tanstack/react-query";

import { bucketsToSparkline } from "../lib/session-strips";
import type { SessionStripData } from "../lib/session-strips";
import { fetchSummary } from "../lib/api";
import { categoryColor, categoryLabel, statusColor } from "../lib/format";

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
  const summaryQ = useQuery({
    queryKey: ["strip-summary", data.sessionId, summaryKind],
    queryFn: ({ signal }) => fetchSummary(data.sessionId, { kind: summaryKind }, signal),
    enabled: !!data.sessionId,
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
      className={`px-3 py-2 border-b border-[#21262d] cursor-pointer flex gap-3 ${
        selected ? "bg-[#1f6feb22]" : "hover:bg-[#1f6feb14]"
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
          <span className="text-[#6e7681] text-[10px]">{statusText}</span>
          <span className="ml-auto text-[#6e7681] text-[10px] whitespace-nowrap">
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
                className="flex-1 truncate text-[#c9d1d9]"
                title={`${row.total} ${categoryLabel(row.category)} events, peak ${row.peak}/min`}
                style={{ letterSpacing: "-0.04em" }}
              >
                {bucketsToSparkline(row.counts, data.peakPerMin || row.peak)}
              </span>
              <span className="shrink-0 text-[9px] text-[#6e7681] tabular-nums">
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
            className="mt-1 text-[10px] text-[#8b949e] leading-tight line-clamp-2"
            title={summaryText}
          >
            <span className="text-[#58a6ff] mr-1 uppercase tracking-wider text-[9px]">
              ai
            </span>
            {summaryText}
          </div>
        ) : null}
        {/* When the summary is loading and we don't yet have text,
            keep the layout calm — show a tiny ghost placeholder so
            the strip doesn't jump when the text arrives. */}
        {!summaryAvailable && !summaryDisabled && !isStuck && summaryQ.isPending ? (
          <div className="mt-1 text-[10px] text-[#484f58] italic leading-tight">
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
            className="mt-1 text-[10px] text-[#484f58] italic leading-tight"
            title={`Source: ${summaryQ.data?.source ?? "unknown"}`}
          >
            ai · no rollup available
            {summaryQ.data?.source ? (
              <span className="text-[#30363d] ml-1">({summaryQ.data.source})</span>
            ) : null}
          </div>
        ) : null}
        {/* Stuck banner — only when the session is in awaiting_user
            past the stuck threshold. Mirrors the EventTicker's
            stuck-reason line styling. Includes the AI "what it's
            waiting on" rollup when available — operator gets both
            the raw question and the LLM rollup in one line. */}
        {data.stuck ? (
          <div
            data-testid="session-strip-stuck"
            className="mt-1 text-[10px] font-bold text-[#f85149] truncate"
            title={data.stuck.question ?? data.stuck.kind ?? "awaiting"}
          >
            ⚠ STUCK {formatStuckAge(data.stuck.ageSecs)} ·{" "}
            {data.stuck.kind ?? "awaiting"}
            {data.stuck.question ? (
              <span className="font-normal text-[#ffa198] ml-1">
                — {data.stuck.question.length > 90 ? `${data.stuck.question.slice(0, 88)}…` : data.stuck.question}
              </span>
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
