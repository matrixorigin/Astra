import { SkeletonPageHeader, SkeletonCardGrid } from '@/components/loading/skeletons';

export default function ModelDetailLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonCardGrid count={4} lines={4} />
    </div>
  );
}
