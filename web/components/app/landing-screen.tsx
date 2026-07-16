import {
  ArrowRight,
  Bot,
  CheckCircle2,
  GitBranch,
  ListTodo,
  ScanSearch,
} from 'lucide-react';
import Link from 'next/link';

const sessionLayers = [
  {
    icon: ListTodo,
    label: 'Tasks',
    title: 'Work stays explicit',
    detail: 'Goals, blockers, and next actions remain resumable.',
  },
  {
    icon: Bot,
    label: 'Agents',
    title: 'Delegation stays visible',
    detail: 'Each child run keeps status, tools, and transcript identity.',
  },
  {
    icon: ScanSearch,
    label: 'Evidence',
    title: 'Decisions stay inspectable',
    detail: 'Audit, reflection, and routing evidence share one session.',
  },
];

export function LandingScreen() {
  return (
    <main className="relative min-h-screen overflow-hidden bg-[#0d1422] text-white">
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_12%_15%,rgba(59,130,246,0.2),transparent_32%),radial-gradient(circle_at_82%_72%,rgba(139,92,246,0.13),transparent_34%)]"
      />
      <div className="relative mx-auto flex min-h-screen max-w-[1240px] flex-col px-6 py-6 sm:px-10 lg:px-14 lg:py-9">
        <header className="flex items-center justify-between">
          <Link
            href="/"
            className="inline-flex items-center gap-2 text-sm font-semibold"
            aria-label="Astra home"
          >
            <span className="flex size-8 items-center justify-center rounded-[8px] bg-white text-xs font-bold text-[#0d1422]">
              A
            </span>
            Astra
          </Link>
          <Link
            href="/login?next=/"
            className="inline-flex h-9 items-center rounded-[8px] border border-white/15 px-3.5 text-sm font-medium text-white/75 transition hover:border-white/25 hover:bg-white/[0.05] hover:text-white"
          >
            Sign in
          </Link>
        </header>

        <div className="grid flex-1 items-center gap-14 py-14 lg:grid-cols-[minmax(0,1fr)_520px]">
          <section className="max-w-2xl">
            <div className="inline-flex items-center gap-2 rounded-full border border-blue-300/15 bg-blue-300/[0.08] px-3 py-1 text-xs font-medium text-blue-200">
              <GitBranch className="size-3.5" />
              Durable agent workspace
            </div>
            <h1 className="mt-6 text-[clamp(3rem,7vw,5.8rem)] font-semibold leading-[0.94] tracking-[-0.055em]">
              Move complex work
              <span className="block text-white/45">without losing the thread.</span>
            </h1>
            <p className="mt-7 max-w-xl text-base leading-7 text-white/60 sm:text-lg">
              Plan, delegate, use tools, inspect decisions, and resume long-running
              work from one coherent session.
            </p>
            <div className="mt-9 flex flex-col gap-3 sm:flex-row">
              <Link
                href="/register"
                className="inline-flex h-11 items-center justify-center gap-2 rounded-[8px] bg-white px-5 text-sm font-semibold text-[#0d1422] transition hover:-translate-y-px hover:bg-blue-100"
              >
                Create workspace
                <ArrowRight className="size-4" />
              </Link>
              <Link
                href="/login?next=/"
                className="inline-flex h-11 items-center justify-center rounded-[8px] border border-white/15 px-5 text-sm font-semibold text-white transition hover:border-white/25 hover:bg-white/[0.05]"
              >
                Continue a session
              </Link>
            </div>
            <div className="mt-9 flex flex-wrap gap-x-5 gap-y-2 text-xs text-white/40">
              <span className="inline-flex items-center gap-1.5">
                <CheckCircle2 className="size-3.5 text-blue-300" />
                Observable multi-agent work
              </span>
              <span className="inline-flex items-center gap-1.5">
                <CheckCircle2 className="size-3.5 text-blue-300" />
                Explicit runtime boundaries
              </span>
              <span className="inline-flex items-center gap-1.5">
                <CheckCircle2 className="size-3.5 text-blue-300" />
                SDK-first integration
              </span>
            </div>
          </section>

          <section
            aria-label="Astra session model"
            className="rounded-[16px] border border-white/10 bg-white/[0.045] p-4 shadow-[0_30px_80px_rgba(0,0,0,0.28)] backdrop-blur"
          >
            <div className="flex items-center justify-between border-b border-white/10 px-1 pb-4">
              <div>
                <p className="text-sm font-semibold">One durable session</p>
                <p className="mt-1 text-xs text-white/40">
                  Conversation and execution stay connected
                </p>
              </div>
              <span className="inline-flex items-center gap-1.5 rounded-full bg-emerald-400/10 px-2.5 py-1 text-[11px] font-medium text-emerald-300">
                <span className="size-1.5 rounded-full bg-emerald-300" />
                Resumable
              </span>
            </div>
            <div className="mt-4 space-y-2">
              {sessionLayers.map((layer, index) => (
                <div
                  key={layer.label}
                  className="flex items-start gap-3 rounded-[10px] border border-white/10 bg-black/10 p-4"
                >
                  <span className="flex size-9 shrink-0 items-center justify-center rounded-[8px] border border-white/10 bg-white/[0.05] text-blue-300">
                    <layer.icon className="size-4" />
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="text-[10px] font-semibold uppercase tracking-[0.12em] text-white/35">
                        {layer.label}
                      </span>
                      <span className="text-[10px] text-white/20">
                        0{index + 1}
                      </span>
                    </div>
                    <p className="mt-1 text-sm font-semibold text-white/90">
                      {layer.title}
                    </p>
                    <p className="mt-1 text-xs leading-5 text-white/45">
                      {layer.detail}
                    </p>
                  </div>
                </div>
              ))}
            </div>
          </section>
        </div>

        <footer className="flex items-center justify-between border-t border-white/10 pt-5 text-[11px] text-white/30">
          <span>Astra</span>
          <span>Auditable · resumable · integration-ready</span>
        </footer>
      </div>
    </main>
  );
}
