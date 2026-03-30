/** Horizontal bar chart for event type breakdown */

type BarItem = {
  label: string;
  count: number;
  color: string;
};

export function EventBreakdownChart({ items }: { items: BarItem[] }) {
  const max = Math.max(...items.map((i) => i.count), 1);

  if (items.length === 0) {
    return (
      <p className="text-sm text-slate-500">No events to display</p>
    );
  }

  return (
    <div className="space-y-2">
      {items.map((item) => (
        <div key={item.label} className="flex items-center gap-3">
          <span className="w-28 shrink-0 truncate text-xs text-slate-400" title={item.label}>
            {item.label}
          </span>
          <div className="flex-1">
            <div className="h-4 overflow-hidden rounded bg-slate-800">
              <div
                className="h-full rounded transition-all duration-300"
                style={{
                  width: `${(item.count / max) * 100}%`,
                  backgroundColor: item.color,
                }}
              />
            </div>
          </div>
          <span className="w-8 text-right text-xs font-medium text-slate-300">{item.count}</span>
        </div>
      ))}
    </div>
  );
}
