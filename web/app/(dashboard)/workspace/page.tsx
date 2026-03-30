import { getWebConfigurationMessage } from '@/lib/api/client';
import { getSessionWorkspace } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { WorkspaceShell } from '@/components/workspace/workspace-shell';
import { getRuntimeConfig } from '@/lib/runtime-config';

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

  if (mode === 'demo') {
    return (
      <SectionCard
        title="Web agent workspace"
        description="The workspace requires a live backend connection for streaming chat."
      >
        <StatusCallout
          title="Demo mode"
          message="The agent workspace requires a live connection to POST /chat/stream. Switch to live mode in Settings to use this feature."
        />
      </SectionCard>
    );
  }

  const resolvedParams = searchParams ? await searchParams : undefined;
  const sessionId = resolvedParams?.sessionId;

  // Load existing session data if a sessionId is provided
  let workspace: Awaited<ReturnType<typeof getSessionWorkspace>> | null = null;
  if (sessionId) {
    try {
      workspace = await getSessionWorkspace(sessionId);
    } catch {
      // Session may not exist — the workspace will create one on first message
    }
  }

  const chatConfig = {
    sessionId: sessionId ?? undefined,
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-lg font-semibold text-white">Agent workspace</h1>
          <p className="text-sm text-slate-400">
            {workspace
              ? `Session: ${workspace.session.title}`
              : 'Start a new conversation or load an existing session via ?sessionId='}
          </p>
        </div>
      </div>

      <WorkspaceShell
        config={chatConfig}
        session={workspace?.session}
        events={workspace?.events}
        reflection={workspace?.reflection}
      />
    </div>
  );
}
