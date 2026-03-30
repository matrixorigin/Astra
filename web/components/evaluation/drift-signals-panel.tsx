import type { DriftData } from '@/lib/models/platform';

const severityClasses: Record<string, string> = {
  critical: 'border-red-400/30 bg-red-400/15 text-red-300',
  warning: 'border-amber-400/30 bg-amber-400/15 text-amber-300',
  info: 'border-sky-400/30 bg-sky-400/15 text-sky-300',
};

const badgeClasses: Record<string, string> = {
  critical: 'bg-red-500/20 text-red-300',
  warning: 'bg-amber-500/20 text-amber-300',
  info: 'bg-sky-500/20 text-sky-300',
};

export function DriftSignalsPanel({ data }: { data: DriftData }) {
  const { signals, checkedAt } = data;

  if (signals.length === 0) {
    return (
      <div className="space-y-3">
        <p className="py-6 text-center text-sm text-slate-500">No drift signals detected.</p>
        <p className="text-center text-xs text-slate-600">Checked at {checkedAt}</p>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <ul className="space-y-2">
        {signals.map((s, i) => (
          <li
            key={`${s.model}-${s.templateId}-${i}`}
            className={`rounded-xl border p-4 ${severityClasses[s.severity] ?? severityClasses.info}`}
          >
            <div className="flex items-start justify-between gap-2">
              <div className="space-y-1">
                <div className="flex items-center gap-2">
                  <span
                    className={`inline-block rounded-full px-2 py-0.5 text-xs font-semibold ${badgeClasses[s.severity] ?? badgeClasses.info}`}
                  >
                    {s.severity}
                  </span>
                  <span className="text-sm font-medium text-white">{s.model}</span>
                </div>
                <p className="text-xs text-slate-400">Template: {s.templateId}</p>
              </div>
              <span className="shrink-0 text-xs text-slate-500">{s.sampleCount} samples</span>
            </div>

            <div className="mt-3 flex items-center gap-4 text-xs">
              <span>
                Previous <strong className="text-white">{s.previousAvg.toFixed(3)}</strong>
              </span>
              <span className="text-slate-600">→</span>
              <span>
                Current <strong className="text-white">{s.currentAvg.toFixed(3)}</strong>
              </span>
              <span
                className={
                  s.delta < 0 ? 'font-semibold text-red-400' : 'font-semibold text-green-400'
                }
              >
                {s.delta >= 0 ? '+' : ''}
                {s.delta.toFixed(3)}
              </span>
            </div>
          </li>
        ))}
      </ul>
      <p className="text-right text-xs text-slate-600">Checked at {checkedAt}</p>
    </div>
  );
}
