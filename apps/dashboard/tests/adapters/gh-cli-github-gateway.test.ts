import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { GhCliGitHubGateway } from "@/adapters/gh-cli-github-gateway";

/**
 * Write a fake `gh` script that echoes a pre-baked JSON response for a
 * specific argv match, otherwise exits non-zero. Tests pass the path to
 * the gateway constructor so no real `gh` is invoked.
 *
 * On Windows we write a .cmd wrapper that delegates to node so we don't
 * need a real shell; on POSIX we write a bash script.
 */
async function writeFakeGh(
  dir: string,
  responseByCommand: Record<string, string>,
): Promise<string> {
  const mapPath = path.join(dir, "responses.json");
  await fs.writeFile(mapPath, JSON.stringify(responseByCommand));
  if (process.platform === "win32") {
    const cmd = path.join(dir, "gh.cmd");
    const node = process.execPath.replace(/\\/g, "\\\\");
    const js = path.join(dir, "gh.js");
    const script = `
const fs = require('fs');
const map = JSON.parse(fs.readFileSync(${JSON.stringify(mapPath)}, 'utf8'));
const argv = process.argv.slice(2);
const command = argv[0];
const body = map[command];
if (body === undefined) {
  console.error('no fake response for command ' + command);
  process.exit(1);
}
process.stdout.write(body);
process.exit(0);
`;
    await fs.writeFile(js, script);
    await fs.writeFile(cmd, `@echo off\r\n"${node}" "${js}" %*\r\n`);
    return cmd;
  }
  const bin = path.join(dir, "gh");
  const node = process.execPath;
  const js = path.join(dir, "gh.js");
  const script = `
const fs = require('fs');
const map = JSON.parse(fs.readFileSync(${JSON.stringify(mapPath)}, 'utf8'));
const argv = process.argv.slice(2);
const command = argv[0];
const body = map[command];
if (body === undefined) {
  console.error('no fake response for command ' + command);
  process.exit(1);
}
process.stdout.write(body);
process.exit(0);
`;
  await fs.writeFile(js, script);
  await fs.writeFile(bin, `#!/usr/bin/env sh\nexec "${node}" "${js}" "$@"\n`);
  await fs.chmod(bin, 0o755);
  return bin;
}

describe("GhCliGitHubGateway", () => {
  let dir: string;

  beforeEach(async () => {
    dir = await fs.mkdtemp(path.join(os.tmpdir(), "sen24-gh-"));
  });

  afterEach(async () => {
    await fs.rm(dir, { recursive: true, force: true });
  });

  it("getMergedPullRequests parses gh JSON and filters by merged window", async () => {
    const ghPath = await writeFakeGh(dir, {
      pr: JSON.stringify([
        {
          number: 1,
          title: "in window",
          mergedAt: "2026-05-10T00:00:00Z",
          author: { login: "alice" },
        },
        {
          number: 2,
          title: "before window",
          mergedAt: "2025-01-01T00:00:00Z",
          author: { login: "alice" },
        },
        {
          number: 3,
          title: "never merged",
          mergedAt: null,
        },
      ]),
    });
    const gw = new GhCliGitHubGateway(ghPath);
    const prs = await gw.getMergedPullRequests(
      {
        start: new Date("2026-01-01T00:00:00Z"),
        end: new Date("2027-01-01T00:00:00Z"),
      },
      ["acme/web"],
    );
    expect(prs).toHaveLength(1);
    expect(prs[0]).toMatchObject({
      number: 1,
      title: "in window",
      repo: "acme/web",
      codexFindings: 0,
      codeRabbitFindings: 0,
    });
  });

  it("getReviewComments merges comments + reviews and flags bots", async () => {
    const ghPath = await writeFakeGh(dir, {
      pr: JSON.stringify({
        comments: [
          {
            author: { login: "garysomerhalder" },
            body: "lgtm",
            createdAt: "2026-05-10T10:00:00Z",
          },
          {
            author: { login: "coderabbitai[bot]" },
            body: "consider X",
            createdAt: "2026-05-10T10:05:00Z",
          },
        ],
        reviews: [
          {
            author: { login: "codex" },
            body: "approve",
            submittedAt: "2026-05-10T10:10:00Z",
          },
          {
            // missing submittedAt → must be dropped
            author: { login: "ghost" },
            body: "no ts",
          },
        ],
      }),
    });
    const gw = new GhCliGitHubGateway(ghPath);
    const got = await gw.getReviewComments(99, "acme/web");
    expect(got).toHaveLength(3);
    expect(got.map((c) => c.author).sort()).toEqual([
      "coderabbitai[bot]",
      "codex",
      "garysomerhalder",
    ]);
    const human = got.find((c) => c.author === "garysomerhalder");
    expect(human?.isBot).toBe(false);
    expect(got.find((c) => c.author === "coderabbitai[bot]")?.isBot).toBe(true);
    expect(got.find((c) => c.author === "codex")?.isBot).toBe(true);
  });

  it("rejects when gh exits non-zero", async () => {
    // No fake responses → fake gh script exits 1.
    const ghPath = await writeFakeGh(dir, {});
    const gw = new GhCliGitHubGateway(ghPath);
    await expect(
      gw.getMergedPullRequests(
        { start: new Date(0), end: new Date() },
        ["acme/web"],
      ),
    ).rejects.toThrow(/gh exited 1/);
  });
});
