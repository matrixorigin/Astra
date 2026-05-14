import { notFound } from 'next/navigation';
import { ProjectDetail } from '@/components/app/project-detail';
import { getCurrentUser } from '@/lib/auth/actions';
import { getProjectHydrated } from '@/lib/api/web-store';

export default async function ProjectPage({
  params,
}: {
  params: Promise<{ projectId: string }>;
}) {
  const user = await getCurrentUser();
  if (!user) {
    notFound();
  }
  const { projectId } = await params;
  const detail = await getProjectHydrated(user.user_id, projectId);
  if (!detail) {
    notFound();
  }
  return <ProjectDetail initial={detail} />;
}
