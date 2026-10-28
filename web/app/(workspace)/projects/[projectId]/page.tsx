import { notFound } from 'next/navigation';
import { ProjectDetail } from '@/components/app/project-detail';
import { currentUserContext } from '@/lib/auth/current-user';
import { getProjectHydrated } from '@/lib/api/web-store';

export default async function ProjectPage({
  params,
}: {
  params: Promise<{ projectId: string }>;
}) {
  const auth = await currentUserContext();
  if (!auth) {
    notFound();
  }
  const { projectId } = await params;
  const detail = await getProjectHydrated(auth.user.user_id, projectId, auth.client);
  if (!detail) {
    notFound();
  }
  return <ProjectDetail initial={detail} />;
}
