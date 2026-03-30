import type { MemoryIntrospectionData } from '@/lib/models/platform';

function StatRow({ label, value }: { label: string; value: string | number | null }) {
  if (value == null) return null;
  return (
    <div className="flex items-center justify-between border-b border-slate-800/50 py-2 last:border-0">
      <span className="text-xs text-slate-500">{label}</span>
      <span className="text-sm font-medium text-white">{value}</span>
    </div>
  );
}

function IntensityBadge({ level }: { level: string }) {
  const colors: Record<string, string> = {
    low: 'bg-green-500/10 text-green-400 border-green-500/30',
    medium: 'bg-amber-500/10 text-amber-400 border-amber-500/30',
    high: 'bg-red-500/10 text-red-400 border-red-500/30',
  };
  return (
    <span className={`rounded-full border px-2 py-0.5 text-xs font-medium ${colors[level] ?? colors.medium}`}>
      {level}
    </span>
  );
}

export function MemoryIntrospectionPanel({ data }: { data: MemoryIntrospectionData }) {
  return (
    <div className="grid gap-4 md:grid-cols-3">
      {/* Episodic memory */}
      <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
        <div className="mb-3 flex items-center justify-between">
          <h4 className="text-sm font-medium text-slate-300">Episodic</h4>
          <IntensityBadge level={data.episodic.toolIntensity} />
        </div>
        <StatRow label="Turns" value={data.episodic.turns} />
        <StatRow label="Total events" value={data.episodic.totalEvents} />
        <StatRow label="Session depth" value={data.episodic.sessionDepth} />
      </div>

      {/* Semantic memory */}
      <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
        <h4 className="mb-3 text-sm font-medium text-slate-300">Semantic</h4>
        <StatRow label="Context snapshots" value={data.semantic.ctxSnapshots} />
        <StatRow label="Peak tokens" value={data.semantic.peakTokens.toLocaleString()} />
        {data.semantic.llmTotalTokens != null && (
          <StatRow label="LLM total tokens" value={data.semantic.llmTotalTokens.toLocaleString()} />
        )}
        {data.semantic.lastAssemblyMs != null && (
          <StatRow label="Assembly latency" value={`${data.semantic.lastAssemblyMs} ms`} />
        )}
      </div>

      {/* Procedural memory */}
      <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
        <h4 className="mb-3 text-sm font-medium text-slate-300">Procedural</h4>
        <StatRow label="Skill selections" value={data.procedural.skillSelections} />
        {data.procedural.accuracyRate != null && (
          <StatRow
            label="Accuracy"
            value={`${(data.procedural.accuracyRate * 100).toFixed(1)}%`}
          />
        )}
      </div>

      {/* Profile summary */}
      {data.profile && data.profile.length > 0 && (
        <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4 md:col-span-3">
          <h4 className="mb-2 text-sm font-medium text-slate-300">Memory profile</h4>
          <div className="flex flex-wrap gap-2">
            {data.profile.map((item, i) => (
              <span key={i} className="rounded-full bg-slate-800 px-3 py-1 text-xs text-slate-300">
                {item}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
