import type { SloDashboardData } from '@/lib/models/platform';

export function SloComplianceTable({ data }: { data: SloDashboardData }) {
  const { agents, periodDays } = data;

  if (agents.length === 0) {
    return (
      <p className="py-6 text-center text-sm text-slate-500">
        No SLO data for the last {periodDays} days.
      </p>
    );
  }

  return (
    <div className="overflow-x-auto">
      <table className="w-full text-left text-sm">
        <thead>
          <tr className="border-b border-slate-800 text-xs uppercase tracking-wider text-slate-500">
            <th className="px-3 py-2">Agent</th>
            <th className="px-3 py-2">SLO</th>
            <th className="px-3 py-2">Target</th>
            <th className="px-3 py-2">Actual</th>
            <th className="px-3 py-2">Progress</th>
            <th className="px-3 py-2 text-center">Met</th>
          </tr>
        </thead>
        <tbody>
          {agents.map((a, i) => {
            const pct = a.target > 0 ? Math.min((a.actual / a.target) * 100, 100) : 0;
            return (
              <tr
                key={`${a.agentId}-${a.sloName}-${i}`}
                className="border-b border-slate-800/50 hover:bg-slate-800/30"
              >
                <td className="px-3 py-2 font-medium text-white">{a.agentId}</td>
                <td className="px-3 py-2 text-slate-300">{a.sloName}</td>
                <td className="px-3 py-2 tabular-nums text-slate-400">
                  {formatValue(a.target)}
                </td>
                <td className="px-3 py-2 tabular-nums text-slate-300">
                  {formatValue(a.actual)}
                </td>
                <td className="px-3 py-2">
                  <div className="h-2 w-24 overflow-hidden rounded-full bg-slate-800">
                    <div
                      className={`h-full rounded-full ${a.met ? 'bg-green-500' : 'bg-amber-500'}`}
                      style={{ width: `${pct}%` }}
                    />
                  </div>
                </td>
                <td className="px-3 py-2 text-center">
                  {a.met ? (
                    <span className="text-green-400" aria-label="Met">
                      ✓
                    </span>
                  ) : (
                    <span className="text-red-400" aria-label="Not met">
                      ✗
                    </span>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function formatValue(v: number): string {
  if (v >= 1 && Number.isInteger(v)) return String(v);
  if (v <= 1) return `${(v * 100).toFixed(1)}%`;
  return v.toFixed(2);
}
