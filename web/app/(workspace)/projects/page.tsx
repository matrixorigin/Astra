import { Suspense } from 'react';
import { ProjectsList } from '@/components/app/projects-list';

export default function ProjectsPage() {
  return (
    <Suspense fallback={<div className="p-8 text-sm text-text-muted">Loading projects...</div>}>
      <ProjectsList />
    </Suspense>
  );
}
