import { SkeletonPageHeader, SkeletonTable } from '@/components/loading/skeletons';

export default function DecisionsLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonTable rows={5} cols={4} />
    </div>
  );
}
