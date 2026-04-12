import Link from 'next/link';
import { getWebConfigurationMessage } from '@/lib/api/client';
import { getOverviewData } from '@/lib/api/platform';
import { SessionFlowGraph } from '@/components/graph/session-flow-graph';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatCard } from '@/components/dashboard/stat-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { StatusBar } from '@/components/dashboard/status-bar';
import { EventBreakdownChart } from '@/components/dashboard/event-breakdown-chart';
import { LiveActivityCard } from '@/components/dashboard/live-activity-card';
import { getRuntimeConfig } from '@/lib/runtime-config';
import type { EventSummary } from '@/lib/models/platform';

export const dynamic = 'force-dynamic';

/* ── Color maps ── */
const sessionStatusColors: Record<string, string> = {
  active: '#0ea5e9',
  paused: '#f59e0b',
  completed: '#22c55e',
  closed: '#6b7280',
  failed: '#ef4444',
  cancelled: '#8b5cf6',
  waiting: '#a78bfa',
};

const eventTypeColors: Record<string, string> = {
  session_start: '#22c55e',
  session_end: '#6b7280',
  turn: '#0ea5e9',
  tool_call: '#f59e0b',
  error: '#ef4444',
  plan_progress: '#8b5cf6',
  agent_delegated: '#a78bfa',
  agent_completed: '#22c55e',
};

function getEventColor(type: string): string {
  if (type === 'agent_completed (failed)') return '#ef4444';
  if (type === 'agent_completed (cancelled)') return '#a78bfa';

  const lower = type.toLowerCase();
  for (const [key, color] of Object.entries(eventTypeColors)) {
    if (lower.includes(key)) return color;
  }
  return '#475569';
}

function getEventBreakdownLabel(event: EventSummary): string {
  if (
    event.type === 'agent_completed' &&
    (event.status === 'completed' ||
      event.status === 'failed' ||
      event.status === 'cancelled')
  ) {
    return `agent_completed (${event.status})`;
  }

  return event.type;
}

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

  // Session status distribution
  const sessionStatusCounts = new Map<string, number>();
  sessions.forEach((s) => sessionStatusCounts.set(s.status, (sessionStatusCounts.get(s.status) ?? 0) + 1));
  const sessionSegments = Array.from(sessionStatusCounts.entries()).map(([label, count]) => ({
    label,
    count,
    color: sessionStatusColors[label] ?? '#475569',
  }));

  // Event type breakdown (top 8)
  const eventTypeCounts = new Map<string, number>();
  events.forEach((event) => {
    const label = getEventBreakdownLabel(event);
    eventTypeCounts.set(label, (eventTypeCounts.get(label) ?? 0) + 1);
  });
  const eventBreakdown = Array.from(eventTypeCounts.entries())
    .sort((a, b) => b[1] - a[1])
    .slice(0, 8)
    .map(([label, count]) => ({ label, count, color: getEventColor(label) }));

  // Agent status distribution
  const activeAgents = agents.filter((a) => a.status === 'active').length;
  const inactiveAgents = agents.length - activeAgents;

  // Synthetic sparkline data (derived from event counts per session)
  const sessionEventSparkline = sessions.slice(0, 8).map((s) => s.eventCount);
  const agentSparkline = agents.slice(0, 8).map((a) => a.skills.length);

  return (
    <div className="space-y-8">
      <section className="grid-pattern rounded-3xl border border-slate-800 bg-slate-950/70 p-8">
        <div className="max-w-3xl space-y-4">
          <span className="inline-flex rounded-full border border-sky-400/30 bg-sky-400/10 px-3 py-1 text-xs font-semibold text-sky-300">
            Platform console
          </span>
          <h1 className="text-4xl font-semibold tracking-tight text-white">
            Agent, session, and event state in one workspace.
          </h1>
          <p className="text-base leading-7 text-slate-300">
            Real-time overview of agents, sessions, events, and system health.
            Click into any card to explore deeper.
          </p>
        </div>
      </section>

      {mode === 'demo' ? (
        <StatusCallout title="Demo data mode" message={config.message} />
      ) : null}

      {/* Stat cards with sparklines */}
      <section className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <StatCard
          label="Active agents"
          value={stats.activeAgents.toString()}
          hint="Agents currently marked active."
          trend={activeAgents > 0 ? 'up' : 'neutral'}
          sparkline={agentSparkline}
        />
        <StatCard
          label="Open sessions"
          value={stats.openSessions.toString()}
          hint="Sessions not yet closed."
          trend={stats.openSessions > 0 ? 'up' : 'neutral'}
          sparkline={sessionEventSparkline}
        />
        <StatCard
          label="Recent events"
          value={stats.recentEvents.toString()}
          hint="Events in the latest snapshot."
          sparkline={sessionEventSparkline.reverse()}
        />
        <StatCard
          label="Persist ok"
          value={stats.persistOk.toString()}
          hint={`Health: ${health.status} · DB: ${health.database}`}
          trend={health.status === 'ok' ? 'up' : 'down'}
        />
      </section>

      {/* Live activity indicator (live mode only) */}
      {mode === 'live' && config.apiUrl ? (
        <section>
          <LiveActivityCard />
        </section>
      ) : null}

      {/* Distribution charts */}
      <section className="grid gap-6 lg:grid-cols-2">
        <SectionCard title="Session status" description="Distribution of session states.">
          {sessionSegments.length > 0 ? (
            <StatusBar segments={sessionSegments} total={sessions.length} />
          ) : (
            <p className="text-sm text-slate-500">No sessions available</p>
          )}
        </SectionCard>

        <SectionCard title="Event breakdown" description="Most common event types in the snapshot.">
          <EventBreakdownChart items={eventBreakdown} />
        </SectionCard>
      </section>

      {/* Flow graph + recent sessions */}
      <section className="grid gap-6 xl:grid-cols-[1.2fr_0.8fr]">
        <SectionCard
          title="Recent sessions"
          description="Latest session containers from the backend."
        >
          <div className="space-y-3">
            {sessions.map((session) => {
              const statusColor = sessionStatusColors[session.status] ?? '#475569';
              return (
                <Link
                  key={session.id}
                  href={`/sessions/${session.id}`}
                  className="block rounded-2xl border border-slate-800 bg-slate-950/70 p-4 transition-colors hover:border-slate-600"
                >
                  <div className="flex flex-wrap items-center justify-between gap-3">
                    <div>
                      <p className="text-sm font-semibold text-white">{session.title}</p>
                      <div className="mt-1 flex items-center gap-2 text-sm text-slate-400">
                        <span
                          className="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium"
                          style={{
                            backgroundColor: `${statusColor}20`,
                            color: statusColor,
                            border: `1px solid ${statusColor}40`,
                          }}
                        >
                          {session.status}
                        </span>
                        <span>{session.owner}</span>
                        <span>·</span>
                        <span>{session.eventCount} events</span>
                      </div>
                    </div>
                    <span className="text-xs text-slate-500">
                      {session.updatedAt ?? session.createdAt}
                    </span>
                  </div>
                </Link>
              );
            })}
          </div>
        </SectionCard>

        <SectionCard
          title="Flow preview"
          description="Event graph for the newest session."
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
              message="Once sessions are available, a graph preview will appear here."
            />
          )}
        </SectionCard>
      </section>

      {/* Bottom grid: agents, sessions, events */}
      <section className="grid gap-6 lg:grid-cols-3">
        <SectionCard title="Agents" description="Configured agent roles.">
          <ul className="space-y-3">
            {agents.map((agent) => (
              <li key={agent.id} className="rounded-2xl border border-slate-800 px-4 py-3">
                <div className="flex items-center justify-between gap-3">
                  <div>
                    <p className="font-medium text-white">{agent.name}</p>
                    <p className="text-sm text-slate-400">
                      {agent.type} · {agent.model}
                    </p>
                    {agent.skills.length > 0 && (
                      <div className="mt-1 flex flex-wrap gap-1">
                        {agent.skills.slice(0, 3).map((skill) => (
                          <span
                            key={skill}
                            className="rounded bg-slate-800 px-1.5 py-0.5 text-[10px] text-slate-400"
                          >
                            {skill}
                          </span>
                        ))}
                        {agent.skills.length > 3 && (
                          <span className="text-[10px] text-slate-500">+{agent.skills.length - 3}</span>
                        )}
                      </div>
                    )}
                  </div>
                  <span className={`text-xs font-medium ${agent.status === 'active' ? 'text-green-400' : 'text-slate-500'}`}>
                    {agent.status}
                  </span>
                </div>
              </li>
            ))}
          </ul>
        </SectionCard>

        <SectionCard title="Sessions" description="Conversation containers.">
          <ul className="space-y-3">
            {sessions.map((session) => (
              <li key={session.id}>
                <Link
                  href={`/sessions/${session.id}`}
                  className="block rounded-2xl border border-slate-800 px-4 py-3 transition-colors hover:border-slate-600"
                >
                  <p className="font-medium text-white">{session.title}</p>
                  <p className="text-sm text-slate-400">
                    {session.status} · {session.owner}
                  </p>
                </Link>
              </li>
            ))}
          </ul>
        </SectionCard>

        <SectionCard title="Events" description="Recent audit records.">
          <ul className="space-y-3">
            {events.map((event) => (
              <li key={event.id} className="rounded-2xl border border-slate-800 px-4 py-3">
                <div className="flex items-center gap-2">
                  <div
                    className="h-2 w-2 shrink-0 rounded-full"
                    style={{ backgroundColor: getEventColor(event.type) }}
                  />
                  <p className="font-medium text-white">{event.type}</p>
                </div>
                <p className="mt-1 truncate text-sm text-slate-400">{event.summary}</p>
              </li>
            ))}
          </ul>
        </SectionCard>
      </section>
    </div>
  );
}
