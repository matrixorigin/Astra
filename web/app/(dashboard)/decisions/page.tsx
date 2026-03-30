import { getWebConfigurationMessage } from '@/lib/api/client';
import { getDecisions } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function DecisionsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Decisions" description="Audit log of agent decisions.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const data = await getDecisions();

  return (
    <SectionCard
      title="Decisions"
      description={`${data.total} decision${data.total === 1 ? '' : 's'} recorded.`}
    >
      {mode === 'demo' ? (
        <div className="mb-5">
          <StatusCallout title="Demo data mode" message={config.message} />
        </div>
      ) : null}

      {data.decisions.length === 0 ? (
        <div className="rounded-2xl border border-dashed border-slate-700 px-6 py-12 text-center">
          <p className="text-sm text-slate-400">No decisions recorded yet.</p>
          <p className="mt-1 text-xs text-slate-500">
            Decision audit entries will appear here once agents start making decisions.
          </p>
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-left text-sm">
            <thead>
              <tr className="border-b border-slate-700 text-xs uppercase tracking-wider text-slate-400">
                <th className="pb-3 pr-4 font-medium">ID</th>
                <th className="pb-3 pr-4 font-medium">Type</th>
                <th className="pb-3 pr-4 font-medium">Status</th>
                <th className="pb-3 font-medium">Timestamp</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-800">
              {data.decisions.map((decision) => (
                <tr key={decision.id} className="transition hover:bg-slate-800/40">
                  <td className="py-3 pr-4 font-mono text-xs text-sky-300">
                    {decision.id.slice(0, 8)}…
                  </td>
                  <td className="py-3 pr-4 text-slate-300">{decision.type}</td>
                  <td className="py-3 pr-4">
                    <span className="inline-flex items-center rounded-full bg-slate-400/10 px-2.5 py-0.5 text-xs font-medium text-slate-300 ring-1 ring-inset ring-slate-400/20">
                      {decision.status}
                    </span>
                  </td>
                  <td className="py-3 font-mono text-xs text-slate-400">
                    {decision.timestamp ? new Date(decision.timestamp).toLocaleString() : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </SectionCard>
  );
}
