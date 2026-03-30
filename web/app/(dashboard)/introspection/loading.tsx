import { SkeletonPageHeader, SkeletonCardGrid } from '@/components/loading/skeletons';

export default function IntrospectionLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonCardGrid count={3} lines={4} />
    </div>
  );
}
