import { Wrench } from 'lucide-react';
import { EmptyState } from '@/components/ui/empty-state';

export default function CustomizePage() {
  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto max-w-5xl">
        <EmptyState
          icon={Wrench}
          title="Customize is coming soon"
          description="Personal skills and workspace preferences will be managed here in a later pass."
        />
      </div>
    </div>
  );
}
