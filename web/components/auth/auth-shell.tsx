import { Bot, GitBranch, ShieldCheck } from "lucide-react";
import Link from "next/link";
import type { ReactNode } from "react";

export function AuthShell({
  eyebrow,
  title,
  description,
  children,
  footer,
}: {
  eyebrow: string;
  title: string;
  description: string;
  children: ReactNode;
  footer: ReactNode;
}) {
  return (
    <main className="grid min-h-screen bg-bg lg:grid-cols-[minmax(0,1fr)_minmax(420px,0.78fr)]">
      <section className="flex min-h-screen flex-col px-6 py-6 sm:px-10 lg:px-14 lg:py-10">
        <Link
          href="/"
          className="inline-flex w-fit items-center gap-2 text-sm font-semibold tracking-[-0.01em] text-text"
          aria-label="Astra home"
        >
          <span className="flex size-8 items-center justify-center rounded-control bg-text text-xs font-semibold text-white">
            A
          </span>
          <span>Astra</span>
        </Link>

        <div className="my-auto w-full max-w-[430px] py-12 lg:ml-[8%]">
          <p className="text-xs font-semibold uppercase tracking-[0.12em] text-accent">
            {eyebrow}
          </p>
          <h1 className="mt-3 text-[clamp(2rem,5vw,3.25rem)] font-semibold leading-[1.04] tracking-[-0.04em] text-text">
            {title}
          </h1>
          <p className="mt-4 max-w-md text-sm leading-6 text-text-secondary">
            {description}
          </p>
          <div className="mt-8">{children}</div>
          <div className="mt-6 text-sm text-text-muted">{footer}</div>
        </div>

        <p className="text-xs text-text-muted">
          Your runtime permissions and workspace authority remain explicit after
          sign-in.
        </p>
      </section>

      <aside className="relative hidden overflow-hidden border-l border-white/10 bg-[#111827] p-10 text-white lg:flex lg:flex-col">
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_20%_15%,rgba(59,130,246,0.22),transparent_34%),radial-gradient(circle_at_90%_80%,rgba(139,92,246,0.16),transparent_36%)]"
        />
        <div className="relative my-auto max-w-lg">
          <p className="text-xs font-semibold uppercase tracking-[0.14em] text-blue-300">
            Agent work you can trust
          </p>
          <h2 className="mt-4 text-4xl font-semibold leading-[1.08] tracking-[-0.04em]">
            Keep the work visible,
            <span className="block text-white/55">not just the answer.</span>
          </h2>
          <p className="mt-5 max-w-md text-sm leading-6 text-white/60">
            Astra keeps tasks, delegated agents, tool routing, transcripts, and
            reflection connected to one durable session.
          </p>
          <div className="mt-10 space-y-3">
            <AuthCapability
              icon={Bot}
              title="Observable agents"
              description="Open each delegated run and inspect its canonical conversation."
            />
            <AuthCapability
              icon={GitBranch}
              title="Durable execution"
              description="Resume long-running work without pretending partial state is complete."
            />
            <AuthCapability
              icon={ShieldCheck}
              title="Explicit boundaries"
              description="See where tools run, what they can access, and when action is required."
            />
          </div>
        </div>
        <p className="relative text-xs text-white/35">
          Auditable · resumable · integration-ready
        </p>
      </aside>
    </main>
  );
}

function AuthCapability({
  icon: Icon,
  title,
  description,
}: {
  icon: typeof Bot;
  title: string;
  description: string;
}) {
  return (
    <div className="flex gap-3 rounded-card border border-white/10 bg-white/[0.045] p-4">
      <span className="flex size-9 shrink-0 items-center justify-center rounded-control border border-white/10 bg-white/[0.06] text-blue-300">
        <Icon className="size-4" />
      </span>
      <div>
        <h3 className="text-sm font-semibold text-white">{title}</h3>
        <p className="mt-1 text-xs leading-5 text-white/50">{description}</p>
      </div>
    </div>
  );
}
