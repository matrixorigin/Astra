import { Archive } from 'lucide-react';
import { EmptyState } from '@/components/ui/empty-state';

export default function ArtifactsPage() {
  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto max-w-5xl">
        <EmptyState
          icon={Archive}
          title="Artifacts are coming soon"
          description="Tool outputs and durable files will appear here once artifact browsing is enabled."
        />
      </div>
    </div>
  );
}
