import { SkeletonPageHeader, SkeletonTable } from '@/components/loading/skeletons';

export default function EventsLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonTable rows={8} cols={4} />
    </div>
  );
}
