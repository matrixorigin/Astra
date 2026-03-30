import Link from 'next/link';
import { getWebConfigurationMessage } from '@/lib/api/client';
import {
  getSessionWorkspace,
  getSessionActivity,
  getDecisionTrace,
  getMemoryIntrospection,
} from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { EventLogViewer } from '@/components/events/event-log-viewer';
import { SessionFlowGraph } from '@/components/graph/session-flow-graph';
import { SessionDetailActions } from '@/components/sessions/session-detail-actions';
import { DecisionTracePanel } from '@/components/sessions/decision-trace-panel';
import { MemoryIntrospectionPanel } from '@/components/sessions/memory-introspection-panel';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function SessionDetailPage({
  params,
}: {
  params: Promise<{ sessionId: string }>;
}) {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Session detail" description="Inspect one session in depth.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const { sessionId } = await params;

  let workspace;
  try {
    workspace = await getSessionWorkspace(sessionId);
  } catch {
    return (
      <SectionCard title="Session not found" description="The requested session could not be loaded.">
        <StatusCallout
          title="Session unavailable"
          message={`Could not load session "${sessionId}". It may not exist or you may need to log in.`}
          tone="warning"
        />
        <div className="mt-4">
          <Link href="/sessions" className="text-sm text-sky-400 hover:text-sky-300">
            ← Back to sessions
          </Link>
        </div>
      </SectionCard>
    );
  }

  const [activity, decisionTrace, memoryData] = await Promise.all([
    getSessionActivity(sessionId).catch(() => null),
    getDecisionTrace(sessionId).catch(() => null),
    getMemoryIntrospection(sessionId).catch(() => null),
  ]);

  return (
    <div className="space-y-6">
      <SectionCard
        title={workspace.session.title}
        description="Session metadata, recent logs, and reflection view."
      >
        {mode === 'demo' ? (
          <div className="mb-5">
            <StatusCallout
              title="Demo data mode"
              message={config.message}
            />
          </div>
        ) : null}

        <div className="grid gap-4 lg:grid-cols-[1fr_280px]">
          <div className="grid gap-4 md:grid-cols-2">
            <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
              <p className="text-xs uppercase tracking-wide text-slate-500">Session id</p>
              <p className="mt-2 text-sm text-white">{workspace.session.id}</p>
            </div>
            <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
              <p className="text-xs uppercase tracking-wide text-slate-500">Owner</p>
              <p className="mt-2 text-sm text-white">{workspace.session.owner}</p>
            </div>
            <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
              <p className="text-xs uppercase tracking-wide text-slate-500">Status</p>
              <p className="mt-2 text-sm text-white">{workspace.session.status}</p>
            </div>
            <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
              <p className="text-xs uppercase tracking-wide text-slate-500">Agent</p>
              <p className="mt-2 text-sm text-white">{workspace.session.agentId ?? 'n/a'}</p>
            </div>
          </div>

          <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
            <p className="text-xs uppercase tracking-wide text-slate-500">Actions</p>
            <div className="mt-4 flex flex-wrap gap-2">
              <Link
                href={`/workspace?sessionId=${workspace.session.id}`}
                className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-200 hover:border-sky-400/40 hover:text-sky-300"
              >
                Open workspace
              </Link>
              <Link
                href="/sessions"
                className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-200 hover:border-sky-400/40 hover:text-sky-300"
              >
                Back to sessions
              </Link>
              {mode !== 'demo' ? (
                <SessionDetailActions session={workspace.session} />
              ) : null}
            </div>
          </div>
        </div>

        {workspace.reflection ? (
          <div className="mt-6 rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
            <p className="text-sm text-slate-400">Reflection</p>
            <p className="mt-2 text-sm leading-6 text-slate-300">{workspace.reflection}</p>
          </div>
        ) : null}

        {workspace.reflectionError ? (
          <div className="mt-6">
            <StatusCallout
              title="Reflection unavailable"
              message={workspace.reflectionError}
              tone="warning"
            />
          </div>
        ) : null}
      </SectionCard>

      {/* Decision trace */}
      {decisionTrace ? (
        <SectionCard
          title="Decision trace"
          description="Root-cause analysis and tool selection insights for this session."
        >
          <DecisionTracePanel data={decisionTrace} />
        </SectionCard>
      ) : null}

      {/* Memory introspection */}
      {memoryData ? (
        <SectionCard
          title="Memory introspection"
          description="Episodic, semantic, and procedural memory statistics."
        >
          <MemoryIntrospectionPanel data={memoryData} />
        </SectionCard>
      ) : null}

      {activity && activity.activities.length > 0 ? (
        <SectionCard
          title="Activity timeline"
          description={`${activity.total} audit entries recorded for this session.`}
        >
          <div className="relative pl-6">
            {/* Timeline rail */}
            <div className="absolute left-[9px] top-0 bottom-0 w-px bg-slate-800" />

            <div className="space-y-1">
              {activity.activities.map((entry, i) => {
                const isLast = i === activity.activities.length - 1;
                const dotColor = entry.action.includes('error') || entry.action.includes('fail')
                  ? 'bg-red-500'
                  : entry.action.includes('complete') || entry.action.includes('success')
                    ? 'bg-green-500'
                    : entry.action.includes('start') || entry.action.includes('create')
                      ? 'bg-sky-500'
                      : 'bg-slate-500';

                return (
                  <div key={entry.logId} className="relative flex items-start gap-3 pb-3">
                    {/* Timeline dot */}
                    <div
                      className={`absolute left-[-15px] top-3 h-2.5 w-2.5 rounded-full ring-2 ring-slate-950 ${dotColor}`}
                    />
                    <div className={`min-w-0 flex-1 rounded-xl border border-slate-800 bg-slate-950/70 p-3 ${isLast ? '' : ''}`}>
                      <div className="flex items-center justify-between gap-2">
                        <p className="text-sm font-medium text-white">{entry.action}</p>
                        <p className="shrink-0 text-xs text-slate-500">{entry.createdAt}</p>
                      </div>
                      {entry.details ? (
                        <p className="mt-1 truncate text-xs text-slate-400">
                          {typeof entry.details === 'string'
                            ? entry.details
                            : JSON.stringify(entry.details)}
                        </p>
                      ) : null}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        </SectionCard>
      ) : null}

      <SectionCard
        title="Session logs"
        description="Filter and inspect the latest event stream for this session."
      >
        <EventLogViewer
          events={workspace.events}
          emptyMessage="No session log entries matched the current filters."
        />
      </SectionCard>

      <SectionCard
        title="Flow and delegation graph"
        description="Visualize how this session progressed through events and agent involvement."
      >
        <SessionFlowGraph
          sessionId={workspace.session.id}
          sessionTitle={workspace.session.title}
          events={workspace.events}
        />
      </SectionCard>
    </div>
  );
}
