import type { Metadata } from "next";
import { Space_Grotesk, Space_Mono, Doto } from "next/font/google";
import "./globals.css";

const spaceGrotesk = Space_Grotesk({
  subsets: ["latin"],
  display: "swap",
  variable: "--font-space-grotesk",
});

const spaceMono = Space_Mono({
  subsets: ["latin"],
  weight: ["400", "700"],
  display: "swap",
  variable: "--font-space-mono",
});

const doto = Doto({
  subsets: ["latin"],
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
      <body>{children}</body>
    </html>
  );
}
