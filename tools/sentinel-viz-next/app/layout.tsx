import type { Metadata } from "next";
import { Space_Grotesk, Space_Mono, Doto } from "next/font/google";
import "./globals.css";

import { Providers } from "../components/Providers";

// Nothing design system fonts. Three families, kept under the
// per-screen "2 families max" budget by reserving Doto strictly
// for hero moments (KpiBar values). Space Grotesk for body/UI,
// Space Mono for data + ALL CAPS labels.
const spaceGrotesk = Space_Grotesk({
  variable: "--font-grotesk",
  subsets: ["latin"],
  weight: ["300", "400", "500", "700"],
  display: "swap",
});

const spaceMono = Space_Mono({
  variable: "--font-space-mono",
  subsets: ["latin"],
  weight: ["400", "700"],
  display: "swap",
});

const doto = Doto({
  variable: "--font-doto",
  subsets: ["latin"],
  weight: ["400", "700"],
  display: "swap",
});

export const metadata: Metadata = {
  title: "sentinel-viz",
  description: "Live Sentinel activity graph viewer",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html
      lang="en"
      className={`${spaceGrotesk.variable} ${spaceMono.variable} ${doto.variable} h-full antialiased`}
    >
      <body className="min-h-full h-screen bg-[var(--black)] text-[var(--text-primary)] font-sans">
        <Providers>{children}</Providers>
      </body>
    </html>
  );
}
