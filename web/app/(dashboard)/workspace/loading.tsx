function Skeleton({ className = '' }: { className?: string }) {
  return <div className={`animate-pulse rounded-xl bg-slate-800/50 ${className}`} />;
}

export default function WorkspaceLoading() {
  return (
    <div className="space-y-4">
      <div className="space-y-2">
        <Skeleton className="h-6 w-48" />
        <Skeleton className="h-4 w-80" />
      </div>
      <Skeleton className="h-[calc(100vh-8rem)]" />
    </div>
  );
}
