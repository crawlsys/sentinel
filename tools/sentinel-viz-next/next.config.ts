import path from "node:path";
import type { NextConfig } from "next";

/// Origin of the Sentinel viz Rust API the browser talks to. Must match
/// lib/api.ts apiBase(). The CSP connect-src has to whitelist this so
/// the EventSource (SSE) and fetch calls aren't blocked. Defaults to the
/// localhost API; override with NEXT_PUBLIC_VIZ_API.
function apiOrigin(): string {
  const raw = process.env.NEXT_PUBLIC_VIZ_API ?? "http://127.0.0.1:8082";
  try {
    return new URL(raw).origin;
  } catch {
    return "http://127.0.0.1:8082";
  }
}

/// Baseline Content-Security-Policy. This binary is a localhost operator
/// tool, so the policy is permissive (it allows inline/eval that the Next
/// dev runtime and d3 inline styles need) but present — it constrains
/// connect-src to self + the known API origin so a compromised dependency
/// can't beacon to an arbitrary host. NOTE: 'unsafe-eval' is required by
/// the Next dev/turbopack runtime; tighten it if this is ever built for a
/// non-dev deployment.
function contentSecurityPolicy(): string {
  const api = apiOrigin();
  return [
    "default-src 'self'",
    "script-src 'self' 'unsafe-inline' 'unsafe-eval'",
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "font-src 'self' data:",
    `connect-src 'self' ${api}`,
    "base-uri 'self'",
    "form-action 'self'",
    "frame-ancestors 'none'",
  ].join("; ");
}

const nextConfig: NextConfig = {
  outputFileTracingRoot: path.resolve(__dirname),
  turbopack: {
    root: path.resolve(__dirname),
  },
  allowedDevOrigins: ["127.0.0.1", "localhost"],
  async headers() {
    return [
      {
        source: "/:path*",
        headers: [
          { key: "Content-Security-Policy", value: contentSecurityPolicy() },
          { key: "X-Content-Type-Options", value: "nosniff" },
        ],
      },
    ];
  },
};

export default nextConfig;
