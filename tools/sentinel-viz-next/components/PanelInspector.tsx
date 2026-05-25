"use client";

import { useQuery } from "@tanstack/react-query";

import type { Node } from "../types/api";
import { fetchActivity } from "../lib/api";
import { relTime, statusColor } from "../lib/format";

interface Props {
  node: Node | null;
  onClose: () => void;
}

export function PanelInspector({ node, onClose }: Props) {
  const sessionId =
    node?.type === "SentinelSession"
      ? (node.data?.session_id as string | undefined)
      : (node?.data?.session_id as string | undefined);

  const activityQ = useQuery({
    queryKey: ["activity", sessionId],
    queryFn: ({ signal }) => fetchActivity(sessionId!, { limit: 10 }, signal),
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
        <h3 className="text-[#58a6ff] text-sm">{node.type}</h3>
        <button
          aria-label="close inspector"
          onClick={onClose}
          className="text-[#6e7681] hover:text-[#c9d1d9]"
        >
          ✕
        </button>
      </header>
      <div className="space-y-1 text-[11px]">
        <Row k="id" v={node.id} />
        {node.session_status ? (
          <div className="flex justify-between">
            <span>session_status</span>
            <span style={{ color: statusColor(node.session_status) }}>{node.session_status}</span>
          </div>
        ) : null}
        {node.last_activity_age_s != null ? (
          <Row k="last_activity" v={`${node.last_activity_age_s}s`} />
        ) : null}
        <Row k="ts" v={node.ts} />
        <Row k="seq" v={String(node.seq)} />
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
          <h4 className="text-[10px] uppercase tracking-wider text-[#6e7681] mb-2">
            recent activity
          </h4>
          {activityQ.isPending ? (
            <p className="text-[#6e7681]">loading…</p>
          ) : activityQ.error ? (
            <p className="text-[#f85149]">activity error</p>
          ) : activityQ.data?.segments.length ? (
            <ul className="space-y-2" data-testid="activity-segments">
              {activityQ.data.segments.slice(-8).reverse().map((s, i) => (
                <li
                  key={`${s.ts}-${i}`}
                  className={`p-2 rounded bg-[#0d1117] border-l-2 ${
                    s.had_error ? "border-[#f85149]" : "border-[#30363d]"
                  }`}
                >
                  <div className="flex justify-between items-baseline text-[10px] mb-1">
                    <span className="text-[#3fb950] font-bold">{s.label}</span>
                    <span className="text-[#6e7681]">{relTime(s.ts)}</span>
                  </div>
                  <div className="text-[10px] text-[#c9d1d9] opacity-90">{s.preview}</div>
                </li>
              ))}
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
