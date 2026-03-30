const base = 'animate-pulse bg-slate-800/50 rounded-xl';

export function SkeletonBox({ className = '' }: { className?: string }) {
  return <div className={`${base} ${className}`} />;
}

export function SkeletonPageHeader() {
  return (
    <div className="space-y-2">
      <SkeletonBox className="h-6 w-48" />
      <SkeletonBox className="h-4 w-72" />
    </div>
  );
}

export function SkeletonStatCards({ count = 4 }: { count?: number }) {
  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
      {Array.from({ length: count }).map((_, i) => (
        <SkeletonBox key={i} className="h-24" />
      ))}
    </div>
  );
}

export function SkeletonTable({ rows = 5, cols = 4 }: { rows?: number; cols?: number }) {
  return (
    <div className="space-y-3">
      <div className="grid gap-4" style={{ gridTemplateColumns: `repeat(${cols}, 1fr)` }}>
        {Array.from({ length: cols }).map((_, i) => (
          <SkeletonBox key={i} className="h-4" />
        ))}
      </div>
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="grid gap-4" style={{ gridTemplateColumns: `repeat(${cols}, 1fr)` }}>
          {Array.from({ length: cols }).map((_, j) => (
            <SkeletonBox key={j} className="h-10" />
          ))}
        </div>
      ))}
    </div>
  );
}

export function SkeletonCard({ lines = 3 }: { lines?: number }) {
  return (
    <div className={`${base} space-y-3 p-6`}>
      <SkeletonBox className="h-4 w-1/3" />
      {Array.from({ length: lines }).map((_, i) => (
        <SkeletonBox key={i} className="h-3 w-full" />
      ))}
    </div>
  );
}

export function SkeletonCardGrid({ count = 4, lines = 3 }: { count?: number; lines?: number }) {
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      {Array.from({ length: count }).map((_, i) => (
        <SkeletonCard key={i} lines={lines} />
      ))}
    </div>
  );
}
