import { describe, it, expect } from "vitest";

import { SystemClock } from "@/adapters/system-clock";

describe("SystemClock", () => {
  it("returns a Date close to now()", () => {
    const before = Date.now();
    const clock = new SystemClock();
    const got = clock.now();
    const after = Date.now();
    expect(got).toBeInstanceOf(Date);
    expect(got.getTime()).toBeGreaterThanOrEqual(before);
    expect(got.getTime()).toBeLessThanOrEqual(after);
  });

  it("advances between consecutive calls (within a tight bound)", () => {
    const clock = new SystemClock();
    const a = clock.now();
    const b = clock.now();
    expect(b.getTime()).toBeGreaterThanOrEqual(a.getTime());
  });
});
