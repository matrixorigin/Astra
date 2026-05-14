import Link from 'next/link';

export function LandingScreen() {
  return (
    <main className="relative flex min-h-screen overflow-hidden bg-[#0a0a0f] px-6 py-10 text-center text-[#f5f5f7] antialiased">
      <div
        aria-hidden="true"
        className="pointer-events-none fixed inset-0 before:absolute before:left-1/2 before:top-1/2 before:h-[900px] before:w-[900px] before:-translate-x-1/2 before:-translate-y-1/2 before:animate-[astraLandingBreathe_8s_ease-in-out_infinite] before:rounded-full before:bg-[radial-gradient(circle,rgba(167,139,250,0.18)_0%,rgba(124,58,237,0.08)_30%,transparent_70%)] before:blur-[40px]"
      />

      <section className="relative z-10 m-auto flex w-full max-w-4xl flex-col items-center">
        <h1 className="animate-[astraLandingFadeUp_1s_ease-out_both] bg-gradient-to-b from-white to-[#a78bfa] bg-clip-text font-serif text-[clamp(72px,14vw,160px)] font-normal leading-none tracking-[0.04em] text-transparent">
          ASTRA
        </h1>

        <div className="mt-7 flex animate-[astraLandingFadeUp_1s_ease-out_0.15s_both] flex-wrap justify-center gap-x-4 gap-y-2 font-mono text-[clamp(12px,1.3vw,14px)] tracking-[0.08em] text-white/55 sm:gap-x-6">
          {['Auditable', 'Safe', 'Trusted', 'Replayable', 'Agent Runtime'].map((word, index) => (
            <span key={word} className="inline-flex items-center gap-4 sm:gap-6">
              {word}
              {index < 4 ? <span className="text-violet-300/50">·</span> : null}
            </span>
          ))}
        </div>

        <p className="mt-10 max-w-2xl animate-[astraLandingFadeUp_1s_ease-out_0.3s_both] text-[clamp(16px,2vw,20px)] font-normal leading-[1.55] text-white/75">
          Where every autonomous decision is
          <br />
          <em className="font-serif text-[1.08em] text-violet-300">
            accountable by infrastructure design.
          </em>
        </p>

        <div className="mt-14 flex w-full max-w-[260px] animate-[astraLandingFadeUp_1s_ease-out_0.45s_both] flex-col gap-3 sm:w-auto sm:max-w-none sm:flex-row">
          <Link
            href="/login?next=/"
            className="inline-flex items-center justify-center gap-2 rounded-full bg-[#f5f5f7] px-8 py-3.5 text-sm font-medium text-[#0a0a0f] transition hover:-translate-y-px hover:bg-violet-300 hover:shadow-[0_10px_30px_rgba(167,139,250,0.3)]"
          >
            Sign in
            <span aria-hidden="true">→</span>
          </Link>
          <Link
            href="/register"
            className="inline-flex items-center justify-center rounded-full border border-white/15 px-8 py-3.5 text-sm font-medium text-white transition hover:border-white/25 hover:bg-white/[0.04]"
          >
            Create account
          </Link>
        </div>
      </section>

      <div className="absolute inset-x-0 bottom-7 animate-[astraLandingFadeUp_1s_ease-out_0.6s_both] text-center font-mono text-[11px] tracking-[0.05em] text-white/30">
        v1.0 · preview
      </div>
    </main>
  );
}
