import type { MemoryHealthData, MemoryMetricsData } from '@/lib/models/platform';

export function MemoryHealthCard({
  health,
  metrics,
}: {
  health: MemoryHealthData | null;
  metrics: MemoryMetricsData | null;
}) {
  if (!health && !metrics) {
    return (
      <p className="py-6 text-center text-sm text-slate-500">
        Memory health data is not available.
      </p>
    );
  }

  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {health && (
        <>
          <Stat
            label="Status"
            value={health.healthy ? 'Healthy' : 'Unhealthy'}
            indicator={health.healthy ? 'green' : 'red'}
          />
          <Stat label="Total memories" value={String(health.totalMemories)} />
          <Stat label="Active memories" value={String(health.activeMemories)} />
          <Stat label="Inactive memories" value={String(health.inactiveMemories)} />
          <Stat
            label="Stale working memories"
            value={String(health.staleWorkingMemories)}
            indicator={health.staleWorkingMemories > 0 ? 'amber' : 'green'}
          />
          <Stat
            label="Orphaned records"
            value={String(health.orphanedRecords)}
            indicator={health.orphanedRecords > 0 ? 'amber' : 'green'}
          />
        </>
      )}
      {metrics && (
        <>
          <Stat label="Avg confidence" value={metrics.avgConfidence.toFixed(2)} />
          <Stat
            label="Stale count"
            value={String(metrics.staleCount)}
            indicator={metrics.staleCount > 0 ? 'amber' : 'green'}
          />
        </>
      )}
    </div>
  );
}

function Stat({
  label,
  value,
  indicator,
}: {
  label: string;
  value: string;
  indicator?: 'green' | 'red' | 'amber';
}) {
  const dotColor =
    indicator === 'green'
      ? 'bg-green-400'
      : indicator === 'red'
        ? 'bg-red-400'
        : indicator === 'amber'
          ? 'bg-amber-400'
          : null;

  return (
    <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
      <p className="text-xs text-slate-500">{label}</p>
      <div className="mt-1 flex items-center gap-2">
        {dotColor && <span className={`inline-block h-2 w-2 rounded-full ${dotColor}`} />}
        <p className="text-lg font-semibold text-white">{value}</p>
      </div>
    </div>
  );
}
