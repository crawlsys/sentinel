// SENTINEL-23 — Deploy event stream port.
//
// Push-based subscription to deploy events. Adapters implement this against
// whatever event source is wired up (webhook bridge, polling, in-process
// pub/sub). `DeployEvent` is also the shape returned by
// `MetricsRepository.readDeploys`, so the type is defined here and re-
// exported through the ports barrel.

/** Cancels a previous `subscribe()` registration. */
export type Unsubscribe = () => void;

/** A single deploy event, regardless of source (CI, webhook, manual). */
export interface DeployEvent {
  readonly timestamp: Date;
  readonly repo: string;
  readonly env: string;
  readonly commit: string;
  readonly durationS: number;
}

export interface DeployEventStream {
  subscribe(handler: (event: DeployEvent) => void): Unsubscribe;
}
