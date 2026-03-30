import { SkeletonPageHeader, SkeletonCardGrid } from '@/components/loading/skeletons';

export default function SettingsLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonCardGrid count={2} lines={5} />
    </div>
  );
}
