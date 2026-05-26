import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { CachedLinearGateway } from "@/adapters/cached-linear-gateway";

const project = "firefly-pro";

async function setupHome(): Promise<string> {
  const home = await fs.mkdtemp(path.join(os.tmpdir(), "sen24linear-"));
  await fs.mkdir(path.join(home, ".claude", "sentinel"), { recursive: true });
  return home;
}

async function writeCache(home: string, rows: unknown[]): Promise<void> {
  await fs.writeFile(
    path.join(home, ".claude", "sentinel", `linear-assigned-${project}.json`),
    JSON.stringify(rows),
  );
}

describe("CachedLinearGateway", () => {
  let home: string;

  beforeEach(async () => {
    home = await setupHome();
  });

  afterEach(async () => {
    await fs.rm(home, { recursive: true, force: true });
  });

  it("returns [] active tickets when cache file is missing", async () => {
    const gw = new CachedLinearGateway(home, project);
    expect(await gw.getActiveTickets()).toEqual([]);
    expect(await gw.getTeams()).toEqual([]);
  });

  it("filters to active status types and maps rows to Ticket[]", async () => {
    await writeCache(home, [
      {
        identifier: "FPCRM-1",
        title: "A",
        status_type: "started",
        priority: 1,
      },
      {
        identifier: "FPCRM-2",
        title: "B",
        status_type: "backlog",
        priority: 3,
      },
      {
        identifier: "FPCRM-3",
        title: "C",
        status_type: "completed",
        priority: 2,
      },
      {
        identifier: "SEN-7",
        title: "D",
        status_type: "canceled",
      },
    ]);
    const gw = new CachedLinearGateway(home, project);
    const tickets = await gw.getActiveTickets();
    expect(tickets.map((t) => String(t.id))).toEqual(["FPCRM-1", "FPCRM-2"]);
    expect(tickets[0]?.priority).toBe("urgent");
    expect(tickets[1]?.priority).toBe("medium");
  });

  it("skips rows with invalid ticket identifiers", async () => {
    await writeCache(home, [
      { identifier: "bad", status_type: "started" },
      { identifier: "FPCRM-1", status_type: "started" },
    ]);
    const gw = new CachedLinearGateway(home, project);
    const tickets = await gw.getActiveTickets();
    expect(tickets).toHaveLength(1);
    expect(String(tickets[0]?.id)).toBe("FPCRM-1");
  });

  it("getTeams derives unique team prefixes from active tickets", async () => {
    await writeCache(home, [
      { identifier: "FPCRM-1", status_type: "started" },
      { identifier: "FPCRM-2", status_type: "started" },
      { identifier: "SEN-3", status_type: "unstarted" },
    ]);
    const gw = new CachedLinearGateway(home, project);
    const teams = (await gw.getTeams()).map(String).sort();
    expect(teams).toEqual(["FPCRM", "SEN"]);
  });

  it("getCompletedTickets returns [] (TODO until history collector lands)", async () => {
    await writeCache(home, [
      { identifier: "FPCRM-1", status_type: "completed" },
    ]);
    const gw = new CachedLinearGateway(home, project);
    expect(
      await gw.getCompletedTickets({
        start: new Date(0),
        end: new Date(),
      }),
    ).toEqual([]);
  });

  it("handles malformed JSON gracefully", async () => {
    await fs.writeFile(
      path.join(home, ".claude", "sentinel", `linear-assigned-${project}.json`),
      "{not-json",
    );
    const gw = new CachedLinearGateway(home, project);
    expect(await gw.getActiveTickets()).toEqual([]);
  });
});
