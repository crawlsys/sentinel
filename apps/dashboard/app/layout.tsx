import type { Metadata } from "next";
import localFont from "next/font/local";
import { ThemeRegistry } from "@/theme/ThemeRegistry";
import "./globals.css";

// Self-hosted via `next/font/local` so the dashboard renders correctly
// in offline / DNS-locked environments (Cloudflare Family DNS, etc.).
// Source TTFs live in `./fonts/` and ship with their OFL licenses; see
// `apps/dashboard/app/fonts/*-OFL.txt` for upstream credits.
const spaceGrotesk = localFont({
  src: "./fonts/SpaceGrotesk-VariableFont.ttf",
  display: "swap",
  variable: "--font-space-grotesk",
});

const spaceMono = localFont({
  src: [
    { path: "./fonts/SpaceMono-Regular.ttf", weight: "400", style: "normal" },
    { path: "./fonts/SpaceMono-Bold.ttf", weight: "700", style: "normal" },
  ],
  display: "swap",
  variable: "--font-space-mono",
});

const doto = localFont({
  src: "./fonts/Doto-VariableFont.ttf",
  display: "swap",
  variable: "--font-doto",
});

export const metadata: Metadata = {
  title: "Sentinel Dashboard",
  description:
    "Sentinel hook engine dashboard — proofs, workflows, metrics, telemetry.",
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  return (
    <html
      lang="en"
      className={`${spaceGrotesk.variable} ${spaceMono.variable} ${doto.variable}`}
    >
      <body>
        <ThemeRegistry>{children}</ThemeRegistry>
      </body>
    </html>
  );
}
