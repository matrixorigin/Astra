/** Horizontal status distribution bar (no external deps) */

type Segment = {
  label: string;
  count: number;
  color: string;
};

export function StatusBar({ segments, total }: { segments: Segment[]; total?: number }) {
  const sum = total ?? segments.reduce((a, b) => a + b.count, 0);
  if (sum === 0) return null;

  return (
    <div>
      <div className="flex h-3 overflow-hidden rounded-full bg-slate-800">
        {segments
          .filter((s) => s.count > 0)
          .map((seg) => (
            <div
              key={seg.label}
              className="transition-all duration-300"
              style={{
                width: `${(seg.count / sum) * 100}%`,
                backgroundColor: seg.color,
              }}
              title={`${seg.label}: ${seg.count}`}
            />
          ))}
      </div>
      <div className="mt-2 flex flex-wrap gap-3">
        {segments
          .filter((s) => s.count > 0)
          .map((seg) => (
            <div key={seg.label} className="flex items-center gap-1.5 text-xs text-slate-400">
              <div className="h-2 w-2 rounded-full" style={{ backgroundColor: seg.color }} />
              <span>{seg.label}</span>
              <span className="font-medium text-slate-300">{seg.count}</span>
            </div>
          ))}
      </div>
    </div>
  );
}
