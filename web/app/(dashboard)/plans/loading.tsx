import { SkeletonPageHeader, SkeletonCard, SkeletonBox } from '@/components/loading/skeletons';

export default function PlansLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonCard lines={1} />
      <SkeletonBox className="h-[50vh]" />
    </div>
  );
}
