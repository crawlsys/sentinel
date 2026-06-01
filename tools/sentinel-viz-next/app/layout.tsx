import type { Metadata } from "next";
import { Geist_Mono } from "next/font/google";
import "./globals.css";

import { Providers } from "../components/Providers";

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

export const metadata: Metadata = {
  title: "sentinel-viz-next",
  description: "Live Sentinel activity graph viewer",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en" className={`${geistMono.variable} h-full antialiased`}>
      <body className="min-h-full h-screen bg-[#0d1117] text-[#c9d1d9]">
        <Providers>{children}</Providers>
      </body>
    </html>
  );
}
