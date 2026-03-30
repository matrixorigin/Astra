export function StatCard({
  label,
  value,
  hint,
  trend,
  sparkline,
}: {
  label: string;
  value: string;
  hint: string;
  trend?: 'up' | 'down' | 'neutral';
  sparkline?: number[];
}) {
  return (
    <div className="rounded-3xl border border-slate-800 bg-slate-900/50 p-5">
      <p className="text-sm text-slate-400">{label}</p>
      <div className="mt-3 flex items-end justify-between gap-3">
        <div className="flex items-baseline gap-2">
          <p className="text-3xl font-semibold text-white">{value}</p>
          {trend && (
            <span className={`text-xs font-medium ${
              trend === 'up' ? 'text-green-400' : trend === 'down' ? 'text-red-400' : 'text-slate-500'
            }`}>
              {trend === 'up' ? '↑' : trend === 'down' ? '↓' : '–'}
            </span>
          )}
        </div>
        {sparkline && sparkline.length > 1 && (
          <MiniSparkline data={sparkline} />
        )}
      </div>
      <p className="mt-3 text-sm leading-6 text-slate-500">{hint}</p>
    </div>
  );
}

function MiniSparkline({ data }: { data: number[] }) {
  const w = 64;
  const h = 24;
  const max = Math.max(...data, 1);
  const min = Math.min(...data, 0);
  const range = max - min || 1;

  const points = data.map((v, i) => {
    const x = (i / (data.length - 1)) * w;
    const y = h - ((v - min) / range) * h;
    return `${x},${y}`;
  });

  const lastVal = data[data.length - 1];
  const prevVal = data[data.length - 2] ?? lastVal;
  const isUp = lastVal >= prevVal;

  return (
    <svg width={w} height={h} className="shrink-0" aria-hidden="true">
      <polyline
        points={points.join(' ')}
        fill="none"
        stroke={isUp ? '#22c55e' : '#ef4444'}
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <circle
        cx={(data.length - 1) / (data.length - 1) * w}
        cy={h - ((lastVal - min) / range) * h}
        r="2"
        fill={isUp ? '#22c55e' : '#ef4444'}
      />
    </svg>
  );
}
