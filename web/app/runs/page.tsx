import { getWebConfigurationMessage } from '@/lib/api/client';
import { getRuns } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

const statusColors: Record<string, string> = {
  running: 'bg-emerald-500/20 text-emerald-300',
  waiting: 'bg-amber-500/20 text-amber-300',
  completed: 'bg-slate-500/20 text-slate-300',
  failed: 'bg-red-500/20 text-red-300',
  cancelled: 'bg-slate-600/20 text-slate-400',
};

export default async function RunsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Runs" description="View and track active and historical runs.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const runList = await getRuns(50);

  return (
    <div className="space-y-6">
      <SectionCard
        title="Runs"
        description={`${runList.total} total run${runList.total !== 1 ? 's' : ''} tracked by the platform.`}
      >
        {mode === 'demo' ? (
          <div className="mb-5">
            <StatusCallout title="Demo data mode" message={config.message} />
          </div>
        ) : null}

        {runList.runs.length === 0 ? (
          <p className="text-sm text-slate-400">No runs found.</p>
        ) : (
          <div className="overflow-hidden rounded-2xl border border-slate-800">
            <table className="min-w-full divide-y divide-slate-800 text-left text-sm">
              <thead className="bg-slate-950/80 text-slate-400">
                <tr>
                  <th className="px-4 py-3 font-medium">Run ID</th>
                  <th className="px-4 py-3 font-medium">Session</th>
                  <th className="px-4 py-3 font-medium">Status</th>
                  <th className="px-4 py-3 font-medium">Events</th>
                  <th className="px-4 py-3 font-medium">Waiting for</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-800 bg-slate-950/40">
                {runList.runs.map((run) => (
                  <tr key={run.runId}>
                    <td className="px-4 py-4">
                      <p className="font-mono text-sm text-white">{run.runId}</p>
                    </td>
                    <td className="px-4 py-4 text-slate-300">{run.sessionId}</td>
                    <td className="px-4 py-4">
                      <span
                        className={`inline-block rounded-full px-2 py-0.5 text-xs font-medium ${statusColors[run.status] ?? 'bg-slate-700/30 text-slate-400'}`}
                      >
                        {run.status}
                      </span>
                    </td>
                    <td className="px-4 py-4 text-slate-300">{run.eventsCount}</td>
                    <td className="px-4 py-4 text-slate-400">{run.waitingFor ?? '—'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </SectionCard>
    </div>
  );
}
