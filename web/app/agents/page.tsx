import { getWebConfigurationMessage } from '@/lib/api/client';
import { getAgents } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
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

      <div className="space-y-4">
        {agents.map((agent) => (
          <div key={agent.id} className="rounded-2xl border border-slate-800 bg-slate-950/70 p-5">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <h2 className="text-lg font-semibold text-white">{agent.name}</h2>
                <p className="text-sm text-slate-400">
                  {agent.type} · model {agent.model}
                </p>
              </div>
              <span className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-300">
                {agent.status}
              </span>
            </div>
            <div className="mt-4 flex flex-wrap gap-2">
              {(agent.skills.length > 0 ? agent.skills : ['No explicit skill list']).map((skill) => (
                <span
                  key={skill}
                  className="rounded-full bg-slate-800 px-3 py-1 text-xs text-slate-300"
                >
                  {skill}
                </span>
              ))}
            </div>
          </div>
        ))}
      </div>
    </SectionCard>
  );
}
