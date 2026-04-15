import { getWebConfigurationMessage } from '@/lib/api/client';
import { getEdges } from '@/lib/api/platform-edges';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { EdgesListClient } from '@/components/edges/edges-list-client';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function EdgesPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard
        title="Edge Agents"
        description="Remote machines connected to the platform as tool executors."
      >
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const edges = await getEdges();

  return (
    <SectionCard
      title="Edge Agents"
      description="Remote machines running astra-edge, providing local tool execution for your sessions."
    >
      {mode === 'demo' ? (
        <div className="mb-5">
          <StatusCallout title="Demo data mode" message={config.message} />
        </div>
      ) : null}

      <EdgesListClient initialEdges={edges} isLive={mode === 'live'} />
    </SectionCard>
  );
}
