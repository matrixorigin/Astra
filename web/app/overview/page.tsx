import { getWebConfigurationMessage } from '@/lib/api/client';
import { getOverviewData } from '@/lib/api/platform';
import { SessionFlowGraph } from '@/components/graph/session-flow-graph';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatCard } from '@/components/dashboard/stat-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function OverviewPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard
        title="Overview"
        description="Connect the frontend to the running API, or enable demo mode for local preview."
      >
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const { health, stats, sessions, agents, events } = await getOverviewData();
  const previewSession = sessions[0];
  const previewEvents = previewSession
    ? events.filter((event) => event.sessionId === previewSession.id)
    : [];

  return (
    <div className="space-y-8">
      <section className="grid-pattern rounded-3xl border border-slate-800 bg-slate-950/70 p-8">
        <div className="max-w-3xl space-y-4">
          <span className="inline-flex rounded-full border border-sky-400/30 bg-sky-400/10 px-3 py-1 text-xs font-semibold text-sky-300">
            Phase 1 observability dashboard
          </span>
          <h1 className="text-4xl font-semibold tracking-tight text-white">
            Agent, session, and event state in one browser workspace.
          </h1>
          <p className="text-base leading-7 text-slate-300">
            This view now prefers real backend data for agents, sessions, events, and health.
            Dedicated run-list APIs are still a backend gap, so overview is currently centered
            on the stable resources that already exist today.
          </p>
        </div>
      </section>

      {mode === 'demo' ? (
        <StatusCallout
          title="Demo data mode"
          message={config.message}
        />
      ) : null}

      <section className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <StatCard label="Active agents" value={stats.activeAgents.toString()} hint="Agents currently marked active in the backend." />
        <StatCard label="Open sessions" value={stats.openSessions.toString()} hint="Sessions not yet closed." />
        <StatCard label="Recent events" value={stats.recentEvents.toString()} hint="Events pulled into the latest dashboard snapshot." />
        <StatCard label="Persist ok" value={stats.persistOk.toString()} hint={`Health: ${health.status} · database ${health.database}.`} />
      </section>

      <section className="grid gap-6 xl:grid-cols-[1.2fr_0.8fr]">
        <SectionCard
          title="Recent sessions"
          description="Latest session containers surfaced from the current backend session list."
        >
          <div className="space-y-3">
            {sessions.map((session) => (
              <div
                key={session.id}
                className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4"
              >
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div>
                    <p className="text-sm font-semibold text-white">{session.title}</p>
                    <p className="mt-1 text-sm text-slate-400">
                      {session.status.toUpperCase()} · {session.owner} · {session.eventCount} events
                    </p>
                  </div>
                  <span className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-300">
                    {session.updatedAt ?? session.createdAt}
                  </span>
                </div>
              </div>
            ))}
          </div>
        </SectionCard>

        <SectionCard
          title="Flow and delegation preview"
          description="A real event-derived graph preview for the newest visible session."
        >
          {previewSession ? (
            <SessionFlowGraph
              sessionId={previewSession.id}
              sessionTitle={previewSession.title}
              events={previewEvents}
            />
          ) : (
            <StatusCallout
              title="No session selected"
              message="Once sessions are available, the overview page will preview the latest session graph here."
            />
          )}
        </SectionCard>
      </section>

      <section className="grid gap-6 lg:grid-cols-3">
        <SectionCard title="Agents" description="Configured agent roles and health.">
          <ul className="space-y-3">
            {agents.map((agent) => (
              <li key={agent.id} className="rounded-2xl border border-slate-800 px-4 py-3">
                <div className="flex items-center justify-between gap-3">
                  <div>
                    <p className="font-medium text-white">{agent.name}</p>
                    <p className="text-sm text-slate-400">
                      {agent.type} · {agent.model}
                    </p>
                  </div>
                  <span className="text-xs text-sky-300">{agent.status}</span>
                </div>
              </li>
            ))}
          </ul>
        </SectionCard>

        <SectionCard title="Sessions" description="Conversation containers and recovery state.">
          <ul className="space-y-3">
            {sessions.map((session) => (
              <li key={session.id} className="rounded-2xl border border-slate-800 px-4 py-3">
                <p className="font-medium text-white">{session.title}</p>
                <p className="text-sm text-slate-400">
                  {session.status} · {session.owner}
                </p>
              </li>
            ))}
          </ul>
        </SectionCard>

        <SectionCard title="Events" description="Most recent audit and streaming records.">
          <ul className="space-y-3">
            {events.map((event) => (
              <li key={event.id} className="rounded-2xl border border-slate-800 px-4 py-3">
                <p className="font-medium text-white">{event.type}</p>
                <p className="text-sm text-slate-400">{event.summary}</p>
              </li>
            ))}
          </ul>
        </SectionCard>
      </section>
    </div>
  );
}
