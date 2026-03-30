import { getWebConfigurationMessage } from '@/lib/api/client';
import { getSessionWorkspace } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { EventLogViewer } from '@/components/events/event-log-viewer';
import { SessionFlowGraph } from '@/components/graph/session-flow-graph';
import { getRuntimeConfig } from '@/lib/runtime-config';

const panes = [
  'Chat thread and streaming output',
  'Tool call timeline',
  'Plan / delegation graph',
  'Session context and memory panel',
  'Approval and intervention controls',
];

export const dynamic = 'force-dynamic';

export default async function WorkspacePage({
  searchParams,
}: {
  searchParams?: Promise<{ sessionId?: string }>;
}) {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard
        title="Web agent workspace"
        description="Connect the frontend to the runtime backend before loading session-centric workspace data."
      >
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const resolvedParams = searchParams ? await searchParams : undefined;
  const sessionId = resolvedParams?.sessionId;

  if (!sessionId) {
    return (
      <SectionCard
        title="Web agent workspace"
        description="Load this page with `?sessionId=<id>` to inspect a live session context."
      >
        <StatusCallout
          title="Session not selected"
          message="This first workspace pass is session-centric. Pick a session ID from the Sessions page and open `/workspace?sessionId=<session_id>`."
        />
      </SectionCard>
    );
  }

  const workspace = await getSessionWorkspace(sessionId);

  return (
    <div className="space-y-6">
      <SectionCard
        title="Web agent workspace"
        description="This route is the shell for the future browser-native agent experience."
      >
        {mode === 'demo' ? (
          <div className="mb-5">
            <StatusCallout
              title="Demo data mode"
              message={config.message}
            />
          </div>
        ) : null}

        <div className="grid gap-4 lg:grid-cols-[1.1fr_0.9fr]">
          <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-5">
            <p className="text-sm text-slate-400">Session context</p>
            <div className="mt-4 grid gap-3 md:grid-cols-2">
              <div className="rounded-xl border border-slate-800 p-4">
                <p className="text-xs uppercase tracking-wide text-slate-500">Title</p>
                <p className="mt-2 text-sm text-white">{workspace.session.title}</p>
              </div>
              <div className="rounded-xl border border-slate-800 p-4">
                <p className="text-xs uppercase tracking-wide text-slate-500">Status</p>
                <p className="mt-2 text-sm text-white">{workspace.session.status}</p>
              </div>
              <div className="rounded-xl border border-slate-800 p-4">
                <p className="text-xs uppercase tracking-wide text-slate-500">Owner</p>
                <p className="mt-2 text-sm text-white">{workspace.session.owner}</p>
              </div>
              <div className="rounded-xl border border-slate-800 p-4">
                <p className="text-xs uppercase tracking-wide text-slate-500">Agent</p>
                <p className="mt-2 text-sm text-white">{workspace.session.agentId ?? 'n/a'}</p>
              </div>
            </div>
          </div>
          <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-5">
            <p className="text-sm text-slate-400">Planned panels</p>
            <ul className="mt-4 space-y-3">
              {panes.map((pane) => (
                <li key={pane} className="rounded-xl border border-slate-800 px-4 py-3 text-sm text-slate-300">
                  {pane}
                </li>
              ))}
            </ul>

            {workspace.reflection ? (
              <div className="mt-6 rounded-2xl border border-slate-800 p-4">
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
          </div>
        </div>
      </SectionCard>

      <SectionCard
        title="Workspace logs"
        description="Live session logs can evolve into streaming panes later; for now this is a filterable event log."
      >
        <EventLogViewer
          events={workspace.events}
          emptyMessage="No workspace log entries matched the current filters."
        />
      </SectionCard>

      <SectionCard
        title="Execution graph"
        description="A graph view over the current session's event flow and agent participation."
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
