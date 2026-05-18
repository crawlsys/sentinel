import { describe, it, expect, vi } from "vitest";

import { InProcessDeployEventStream } from "@/adapters/in-process-deploy-event-stream";
import type { DeployEvent } from "@/ports";

const fixtureEvent = (): DeployEvent => ({
  timestamp: new Date("2026-05-18T12:00:00Z"),
  repo: "sentinel",
  env: "prod",
  commit: "abc123",
  durationS: 42,
});

describe("InProcessDeployEventStream", () => {
  it("delivers published events to every subscriber", () => {
    const stream = new InProcessDeployEventStream();
    const a = vi.fn();
    const b = vi.fn();
    stream.subscribe(a);
    stream.subscribe(b);
    const evt = fixtureEvent();
    stream.publish(evt);
    expect(a).toHaveBeenCalledTimes(1);
    expect(a).toHaveBeenCalledWith(evt);
    expect(b).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledWith(evt);
  });

  it("stops delivering after unsubscribe", () => {
    const stream = new InProcessDeployEventStream();
    const handler = vi.fn();
    const unsubscribe = stream.subscribe(handler);
    stream.publish(fixtureEvent());
    expect(handler).toHaveBeenCalledTimes(1);
    unsubscribe();
    stream.publish(fixtureEvent());
    expect(handler).toHaveBeenCalledTimes(1);
  });

  it("publish with no subscribers is a no-op", () => {
    const stream = new InProcessDeployEventStream();
    expect(() => stream.publish(fixtureEvent())).not.toThrow();
  });
});
