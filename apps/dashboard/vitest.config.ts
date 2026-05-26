import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { resolve } from "path";

export default defineConfig({
  plugins: [react()],
  test: {
    globals: true,
    environment: "jsdom",
    include: ["tests/**/*.{test,spec}.{ts,tsx}", "src/**/*.{test,spec}.{ts,tsx}"],
  },
  resolve: {
    alias: {
      // Order matters: most-specific first. The `@/app/*` mapping mirrors
      // tsconfig.json so SEN-30's e2e test can import the page module
      // (`@/app/page`) the same way Next sees it. Without this entry vitest
      // would resolve `@/app/page` as `src/app/page`, which doesn't exist.
      "@/app": resolve(__dirname, "./app"),
      "@": resolve(__dirname, "./src"),
    },
  },
});
