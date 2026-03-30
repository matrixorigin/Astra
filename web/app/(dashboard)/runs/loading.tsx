import { SkeletonPageHeader, SkeletonTable } from '@/components/loading/skeletons';

export default function RunsLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonTable rows={6} cols={4} />
    </div>
  );
}
