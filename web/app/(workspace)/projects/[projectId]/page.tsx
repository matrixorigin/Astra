import { notFound } from 'next/navigation';
import { ProjectDetail } from '@/components/app/project-detail';
import { getProject } from '@/lib/api/web-store';

export default async function ProjectPage({
  params,
}: {
  params: Promise<{ projectId: string }>;
}) {
  const { projectId } = await params;
  const detail = getProject(projectId);
  if (!detail) {
    notFound();
  }
  return <ProjectDetail initial={detail} />;
}
