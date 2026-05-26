"use client";

import { bucketsToSparkline } from "../lib/session-strips";
import type { SessionStripData } from "../lib/session-strips";
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

        {/* Stuck banner — only when the session is in awaiting_user
            past the stuck threshold. Mirrors the EventTicker's
            stuck-reason line styling. */}
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
