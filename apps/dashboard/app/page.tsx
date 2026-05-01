export default function Home() {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center p-8">
      <h1
        className="text-6xl tracking-widest"
        style={{ fontFamily: "var(--font-doto)" }}
      >
        DASHBOARD
      </h1>
      <p
        className="mt-4 text-sm text-neutral-400"
        style={{ fontFamily: "var(--font-space-grotesk)" }}
      >
        Sentinel hook engine — scaffold (SENTINEL-20)
      </p>
    </main>
  );
}
