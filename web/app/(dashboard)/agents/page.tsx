import { getWebConfigurationMessage } from '@/lib/api/client';
import { getAgents } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { AgentsTableClient } from '@/components/agents/agents-table-client';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function AgentsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Agents" description="List agent profiles from the runtime backend.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const agents = await getAgents();

  return (
    <SectionCard
      title="Agents"
      description="Role, model, delegation ability, and current health for each agent profile."
    >
      {mode === 'demo' ? (
        <div className="mb-5">
          <StatusCallout
            title="Demo data mode"
            message={config.message}
          />
        </div>
      ) : null}

      <AgentsTableClient agents={agents} />
    </SectionCard>
  );
}
