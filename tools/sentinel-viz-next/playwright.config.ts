import { defineConfig } from "@playwright/test";

/// Phase-1 smoke. Assumes you have BOTH services running:
///   $ cd ../sentinel-viz-api && cargo run            # :8082
///   $ cd ../sentinel-viz-next && pnpm dev -p 8083    # :8083
/// Override the targets via PLAYWRIGHT_BASE_URL + NEXT_PUBLIC_VIZ_API.
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: false,
  reporter: "line",
  use: {
    baseURL: process.env.PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:8083",
    actionTimeout: 5_000,
    navigationTimeout: 10_000,
  },
});
