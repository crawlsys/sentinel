/**
 * Atom-tier components for the Nothing-aesthetic dashboard.
 *
 * Atoms are dumb, prop-driven, fully theme-driven leaf components. They
 * MUST NOT import from molecules/, organisms/, templates/, application/,
 * or adapters/ — see `eslint.config.mjs` for the enforced boundary.
 */
export { MetricNumber } from "./MetricNumber";
export type {
  MetricNumberProps,
  MetricFont,
  MetricSize,
} from "./MetricNumber";

export { Label } from "./Label";
export type { LabelProps, LabelTone } from "./Label";

export { StatusDot } from "./StatusDot";
export type { StatusDotProps, StatusTone } from "./StatusDot";

export { SegmentedBar } from "./SegmentedBar";
export type { SegmentedBarProps, SegmentedBarTone } from "./SegmentedBar";

export { Sparkline } from "./Sparkline";
export type { SparklineProps, SparklineTone } from "./Sparkline";

export { Tag } from "./Tag";
export type { TagProps, TagTone } from "./Tag";
