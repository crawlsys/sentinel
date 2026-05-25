"use client";

import { useQuery } from "@tanstack/react-query";

import type { Node } from "../types/api";
import { fetchActivity } from "../lib/api";
import { categoryColor, categoryLabel, relTime, statusColor } from "../lib/format";

function friendlyTitle(node: Node): string {
  switch (node.type) {
    case "SentinelSession":
      return "session";
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
  if (hadError) return { border: "#f85149", label: "#f85149", bg: "#3a0f0f" };
  if (kind === "user_input") return { border: "#58a6ff", label: "#58a6ff", bg: "#0d1f3a" };
  // Assistant turn — color by the dominant tool category within the turn.
  if (tools.length > 0) {
    const t = tools[0];
    if (TC_TOOLS.has(t)) return { border: "#3fb950", label: "#3fb950", bg: "#0d1f0d" };
    if (PLANNING_TOOLS.has(t)) return { border: "#d29922", label: "#d29922", bg: "#1f1a08" };
    if (COMMUNICATION_TOOLS.has(t)) return { border: "#bc8cff", label: "#bc8cff", bg: "#1a0f24" };
  }
  // Text-only assistant turn (no tool use).
  return { border: "#bc8cff", label: "#bc8cff", bg: "#1a0f24" };
}

interface Props {
  node: Node | null;
  /** Anchor timestamp from the ticker click. When set, activity is
   * fetched as a ±60s window around it instead of just the tail. */
  anchorTs?: string | null;
  onClose: () => void;
}

export function PanelInspector({ node, anchorTs, onClose }: Props) {
  const sessionId =
    node?.type === "SentinelSession"
      ? (node.data?.session_id as string | undefined)
      : (node?.data?.session_id as string | undefined);

  const activityQ = useQuery({
    queryKey: ["activity", sessionId, anchorTs ?? null],
    queryFn: ({ signal }) =>
      fetchActivity(
        sessionId!,
        anchorTs ? { limit: 30, atTs: anchorTs, windowSecs: 60 } : { limit: 10 },
        signal,
      ),
    enabled: !!sessionId,
    staleTime: 5_000,
  });

  if (!node) {
    return (
      <section
        data-testid="panel-inspector"
        className="w-[360px] border-l border-[#30363d] bg-[#161b22] text-[#c9d1d9] p-4 text-xs font-mono"
      >
        <p className="text-[#6e7681]">click a node or ticker row to inspect</p>
      </section>
    );
  }

  return (
    <section
      data-testid="panel-inspector"
      className="w-[360px] border-l border-[#30363d] bg-[#161b22] text-[#c9d1d9] p-4 text-xs font-mono overflow-y-auto"
    >
      <header className="flex items-baseline justify-between mb-3">
        <h3 className="text-[#c9d1d9] text-sm">
          <span className="text-[#58a6ff]">{friendlyTitle(node)}</span>
        </h3>
        <button
          aria-label="close inspector"
          onClick={onClose}
          className="text-[#6e7681] hover:text-[#c9d1d9]"
        >
          ✕
        </button>
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
            <span style={{ color: statusColor(node.session_status) }}>{node.session_status}</span>
          </div>
        ) : null}
        {node.last_activity_age_s != null ? (
          <Row k="last activity" v={relTime(new Date(Date.now() - node.last_activity_age_s * 1000).toISOString())} />
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

      {node.awaiting_question ? (
        <div className="mt-4 p-2 bg-[#0d1117] border border-[#30363d] rounded">
          <div className="text-[10px] uppercase text-[#bc8cff] mb-1">awaiting user · {node.awaiting_kind}</div>
          <div className="text-[11px] whitespace-pre-wrap">{node.awaiting_question}</div>
        </div>
      ) : null}

      <details className="mt-4">
        <summary className="cursor-pointer text-[#6e7681] text-[10px] uppercase tracking-wider">data</summary>
        <pre className="mt-2 text-[10px] bg-[#0d1117] p-2 rounded border border-[#30363d] overflow-x-auto">
          {JSON.stringify(node.data, null, 2)}
        </pre>
      </details>

      {sessionId ? (
        <div className="mt-4 border-t border-[#30363d] pt-3">
          <h4 className="text-[10px] uppercase tracking-wider text-[#6e7681] mb-2 flex justify-between">
            <span>{anchorTs ? "activity ± 60s" : "recent activity"}</span>
            {anchorTs ? <span className="text-[#58a6ff]">@ {anchorTs.slice(11, 19)}</span> : null}
          </h4>
          {activityQ.isPending ? (
            <p className="text-[#6e7681]">loading…</p>
          ) : activityQ.error ? (
            <p className="text-[#f85149]">activity error</p>
          ) : activityQ.data?.segments.length ? (
            <ul className="space-y-2" data-testid="activity-segments">
              {activityQ.data.segments.slice(-12).reverse().map((s, i) => {
                const sty = segmentStyle(s.kind, !!s.had_error, s.tools ?? []);
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
                      <span className="text-[#6e7681] whitespace-nowrap">{relTime(s.ts)}</span>
                    </div>
                    <div className="text-[10px] text-[#c9d1d9] opacity-90 whitespace-pre-wrap break-words">
                      {s.preview}
                    </div>
                  </li>
                );
              })}
            </ul>
          ) : (
            <p className="text-[#6e7681]">no segments in window</p>
          )}
        </div>
      ) : null}
    </section>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-[#6e7681]">{k}</span>
      <span className="text-[#58a6ff] truncate">{v}</span>
    </div>
  );
}
