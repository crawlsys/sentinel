"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import type { GetTokenEconomicsResult } from "@/application";
import { SegmentedBar } from "@/components/atoms";
import { MetricCard, type MetricCardTone } from "@/components/molecules";

/** Props for {@link TokenEconomicsPanel}. */
export interface TokenEconomicsPanelProps {
  readonly result: GetTokenEconomicsResult;
}

function fmtUsd(n: number): string {
  if (!Number.isFinite(n)) return "—";
  return `$${Math.round(n).toLocaleString()}`;
}

function toneForCacheHit(rate: number): MetricCardTone {
  if (rate >= 0.6) return "success";
  if (rate >= 0.3) return "warn";
  return "error";
}

/**
 * Token-economics panel: total spend + cache hit rate, with a segmented
 * bar reinforcing the cache-hit value visually.
 */
export function TokenEconomicsPanel(
  props: TokenEconomicsPanelProps,
): ReactElement {
  const { result } = props;
  const cachePct = Math.round(result.cacheHitRate * 100);
  const tone = toneForCacheHit(result.cacheHitRate);
  return (
    <Box
      data-testid="token-economics-panel"
      sx={{
        display: "flex",
        flexDirection: "column",
        gap: 1.5,
      }}
    >
      <Box
        sx={{
          display: "grid",
          gridTemplateColumns: { xs: "1fr", sm: "repeat(2, 1fr)" },
          gap: 2,
        }}
      >
        <MetricCard
          label="TOTAL SPEND"
          value={fmtUsd(result.totalCostUsd as number)}
          tone="primary"
        />
        <MetricCard label="CACHE HIT" value={cachePct} unit="%" tone={tone} />
      </Box>
      <Box>
        <SegmentedBar
          value={result.cacheHitRate}
          tone={tone === "primary" ? "primary" : tone}
          segments={30}
        />
      </Box>
    </Box>
  );
}
