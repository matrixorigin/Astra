'use client';

import type { QualityTrendData } from '@/lib/models/platform';

const CHART_W = 720;
const CHART_H = 260;
const PAD = { top: 20, right: 24, bottom: 36, left: 48 };

export function QualityTrendChart({ data }: { data: QualityTrendData }) {
  const { points, overallAvg, totalEvents } = data;

  if (points.length === 0) {
    return (
      <p className="py-8 text-center text-sm text-slate-500">
        No quality data points available yet.
      </p>
    );
  }

  const scores = points.map((p) => p.avgScore);
  const minScore = Math.min(...scores, 0);
  const maxScore = Math.max(...scores, 1);
  const range = maxScore - minScore || 1;

  const innerW = CHART_W - PAD.left - PAD.right;
  const innerH = CHART_H - PAD.top - PAD.bottom;

  function x(i: number) {
    return PAD.left + (points.length > 1 ? (i / (points.length - 1)) * innerW : innerW / 2);
  }
  function y(v: number) {
    return PAD.top + innerH - ((v - minScore) / range) * innerH;
  }

  const polyline = points.map((p, i) => `${x(i)},${y(p.avgScore)}`).join(' ');
  const avgY = y(overallAvg);

  // Grid lines at 25%, 50%, 75%
  const gridValues = [0.25, 0.5, 0.75].map((f) => minScore + range * f);

  // Show up to 6 x-axis labels
  const labelStep = Math.max(1, Math.floor(points.length / 6));
  const xLabels = points.filter((_, i) => i % labelStep === 0 || i === points.length - 1);

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-sm text-slate-400">Score trend over time</p>
        <span className="rounded-full bg-sky-500/20 px-3 py-0.5 text-xs font-medium text-sky-300">
          {totalEvents} events
        </span>
      </div>

      <svg
        viewBox={`0 0 ${CHART_W} ${CHART_H}`}
        className="w-full"
        aria-label="Quality trend chart"
      >
        {/* Grid lines */}
        {gridValues.map((v) => (
          <g key={v}>
            <line
              x1={PAD.left}
              x2={CHART_W - PAD.right}
              y1={y(v)}
              y2={y(v)}
              stroke="#334155"
              strokeWidth="1"
            />
            <text x={PAD.left - 6} y={y(v) + 4} textAnchor="end" fill="#64748b" fontSize="10">
              {v.toFixed(2)}
            </text>
          </g>
        ))}

        {/* Overall average dashed line */}
        <line
          x1={PAD.left}
          x2={CHART_W - PAD.right}
          y1={avgY}
          y2={avgY}
          stroke="#f59e0b"
          strokeWidth="1"
          strokeDasharray="6 4"
        />
        <text
          x={CHART_W - PAD.right + 4}
          y={avgY + 4}
          fill="#f59e0b"
          fontSize="10"
        >
          avg {overallAvg.toFixed(2)}
        </text>

        {/* Data line */}
        <polyline
          points={polyline}
          fill="none"
          stroke="#38bdf8"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
        />

        {/* Data points */}
        {points.map((p, i) => (
          <circle key={p.date} cx={x(i)} cy={y(p.avgScore)} r="3" fill="#38bdf8">
            <title>{`${p.date}: ${p.avgScore.toFixed(3)} (${p.count} samples, ${p.model})`}</title>
          </circle>
        ))}

        {/* X-axis labels */}
        {xLabels.map((p) => {
          const idx = points.indexOf(p);
          return (
            <text
              key={p.date}
              x={x(idx)}
              y={CHART_H - 6}
              textAnchor="middle"
              fill="#64748b"
              fontSize="10"
            >
              {p.date.slice(5)}
            </text>
          );
        })}
      </svg>
    </div>
  );
}
