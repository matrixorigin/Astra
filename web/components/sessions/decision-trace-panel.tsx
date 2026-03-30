'use client';

import { useState } from 'react';
import type { DecisionTraceData, Diagnosis, Insight } from '@/lib/models/platform';

const severityColors = {
  critical: { bg: 'bg-red-500/10', border: 'border-red-500/30', text: 'text-red-400' },
  warning: { bg: 'bg-amber-500/10', border: 'border-amber-500/30', text: 'text-amber-400' },
  info: { bg: 'bg-sky-500/10', border: 'border-sky-500/30', text: 'text-sky-400' },
};

function SeverityBadge({ severity }: { severity: string }) {
  const colors = severityColors[severity as keyof typeof severityColors] ?? severityColors.info;
  return (
    <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${colors.bg} ${colors.border} ${colors.text} border`}>
      {severity}
    </span>
  );
}

function DiagnosisCard({ d }: { d: Diagnosis }) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <SeverityBadge severity={d.severity} />
            <span className="text-xs text-slate-500">{d.category}</span>
            <span className="text-xs text-slate-600">× {d.occurrences}</span>
          </div>
          <p className="mt-2 text-sm text-white">{d.summary}</p>
          {d.affectedTool && (
            <p className="mt-1 text-xs text-slate-400">Tool: <code className="text-slate-300">{d.affectedTool}</code></p>
          )}
        </div>
      </div>
      {d.fixHint && (
        <p className="mt-2 rounded-lg bg-slate-900 px-3 py-2 text-xs text-emerald-400">
          💡 {d.fixHint}
        </p>
      )}
      {d.samples.length > 0 && (
        <div className="mt-2">
          <button
            onClick={() => setExpanded(!expanded)}
            className="text-xs text-slate-500 hover:text-slate-300"
          >
            {expanded ? '▾ Hide samples' : `▸ Show ${d.samples.length} sample(s)`}
          </button>
          {expanded && (
            <div className="mt-2 space-y-1">
              {d.samples.map((s, i) => (
                <pre key={i} className="overflow-x-auto rounded bg-slate-900 px-3 py-1.5 text-xs text-slate-400">
                  {s}
                </pre>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function InsightCard({ insight }: { insight: Insight }) {
  return (
    <div className="flex items-start gap-3 rounded-xl border border-slate-800 bg-slate-950/70 p-3">
      <SeverityBadge severity={insight.severity} />
      <div className="min-w-0">
        <p className="text-sm text-white">{insight.message}</p>
        {insight.evidence && (
          <p className="mt-1 text-xs text-slate-500">{insight.evidence}</p>
        )}
      </div>
    </div>
  );
}

export function DecisionTracePanel({ data }: { data: DecisionTraceData }) {
  const ov = data.overview;

  return (
    <div className="space-y-5">
      {/* Overview grid */}
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        {[
          { label: 'Events', value: ov.totalEvents },
          { label: 'Decisions', value: ov.totalDecisions },
          { label: 'Errors', value: ov.errorCount, warn: ov.errorCount > 0 },
          { label: 'Error rate', value: `${ov.errorRatePct.toFixed(1)}%`, warn: ov.errorRatePct > 10 },
        ].map(({ label, value, warn }) => (
          <div key={label} className="rounded-xl border border-slate-800 bg-slate-950/70 p-3 text-center">
            <p className="text-xs text-slate-500">{label}</p>
            <p className={`mt-1 text-lg font-semibold ${warn ? 'text-red-400' : 'text-white'}`}>
              {value}
            </p>
          </div>
        ))}
      </div>

      {/* Duration & skills */}
      {(ov.durationMinutes || ov.uniqueSkillsUsed > 0 || ov.topSkills.length > 0) && (
        <div className="flex flex-wrap gap-4 text-xs text-slate-400">
          {ov.durationMinutes != null && (
            <span>⏱ {ov.durationMinutes.toFixed(1)} min</span>
          )}
          {ov.uniqueSkillsUsed > 0 && (
            <span>🔧 {ov.uniqueSkillsUsed} unique skills</span>
          )}
          {ov.topSkills.length > 0 && (
            <span>
              Top skills: {ov.topSkills.slice(0, 5).map(([name, count]) => `${name} (${count})`).join(', ')}
            </span>
          )}
        </div>
      )}

      {/* Diagnoses */}
      {data.diagnoses.length > 0 && (
        <div>
          <h3 className="mb-3 text-sm font-medium text-slate-300">
            Diagnoses ({data.diagnoses.length})
          </h3>
          <div className="space-y-3">
            {data.diagnoses.map((d, i) => (
              <DiagnosisCard key={i} d={d} />
            ))}
          </div>
        </div>
      )}

      {/* Insights */}
      {data.insights.length > 0 && (
        <div>
          <h3 className="mb-3 text-sm font-medium text-slate-300">
            Insights ({data.insights.length})
          </h3>
          <div className="space-y-2">
            {data.insights.map((insight, i) => (
              <InsightCard key={i} insight={insight} />
            ))}
          </div>
        </div>
      )}

      {/* Recommendations */}
      {data.recommendations.length > 0 && (
        <div>
          <h3 className="mb-3 text-sm font-medium text-slate-300">Recommendations</h3>
          <ul className="space-y-1">
            {data.recommendations.map((rec, i) => (
              <li key={i} className="flex items-start gap-2 text-sm text-slate-400">
                <span className="mt-0.5 text-emerald-500">→</span>
                <span>{rec}</span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {data.diagnoses.length === 0 && data.insights.length === 0 && data.recommendations.length === 0 && (
        <p className="text-center text-sm text-slate-500">
          No issues detected in this session. Everything looks healthy!
        </p>
      )}
    </div>
  );
}
