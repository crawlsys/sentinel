import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  experimental: {
    // App Router is stable in Next 15; no flags needed for now.
  },
};

export default nextConfig;
