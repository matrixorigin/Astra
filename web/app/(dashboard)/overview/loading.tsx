import { SkeletonPageHeader, SkeletonStatCards, SkeletonCard } from '@/components/loading/skeletons';

export default function OverviewLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonStatCards count={4} />
      <div className="grid gap-6 lg:grid-cols-2">
        <SkeletonCard />
        <SkeletonCard />
      </div>
    </div>
  );
}
