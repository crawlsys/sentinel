// SENTINEL-24 — In-process deploy event stream adapter.
//
// Degenerate DeployEventStream implementation backed by an internal
// Set<handler>. Webhook intake / CI integration is a follow-up; this
// adapter exists so the application layer (SEN-25) can be unit-tested
// against a real subscribe/publish round-trip without an external broker.

import type { DeployEvent, DeployEventStream, Unsubscribe } from "../ports";

type Handler = (event: DeployEvent) => void;

export class InProcessDeployEventStream implements DeployEventStream {
  private readonly handlers = new Set<Handler>();

  subscribe(handler: Handler): Unsubscribe {
    this.handlers.add(handler);
    return () => {
      this.handlers.delete(handler);
    };
  }

  /** Dispatch an event to every active subscriber. Synchronous. */
  publish(event: DeployEvent): void {
    for (const handler of this.handlers) {
      handler(event);
    }
  }
}
