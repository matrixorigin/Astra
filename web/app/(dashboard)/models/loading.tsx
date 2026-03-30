import { SkeletonPageHeader, SkeletonTable } from '@/components/loading/skeletons';

export default function ModelsLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonTable rows={5} cols={5} />
    </div>
  );
}
