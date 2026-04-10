export default function Home() {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-kairo-bg">
      <div className="text-center">
        <h1 className="text-4xl font-bold tracking-tight text-kairo-text">
          K<span className="text-kairo-accent">AI</span>ro
        </h1>
        <p className="mt-4 text-kairo-muted">pre-alpha scaffolding</p>
        <div className="mt-8 flex items-center gap-2">
          <div className="h-2 w-2 rounded-full bg-kairo-accent animate-pulse" />
          <span className="text-sm text-kairo-muted">
            waiting for core runtime
          </span>
        </div>
      </div>
    </main>
  );
}
