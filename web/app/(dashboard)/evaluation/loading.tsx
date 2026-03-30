import { SkeletonPageHeader, SkeletonCard } from '@/components/loading/skeletons';

export default function EvaluationLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonCard lines={6} />
      <div className="grid gap-4 sm:grid-cols-2">
        <SkeletonCard lines={4} />
        <SkeletonCard lines={4} />
      </div>
    </div>
  );
}
