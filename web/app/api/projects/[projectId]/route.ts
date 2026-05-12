import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { getProjectHydrated, updateProject } from '@/lib/api/web-store';
import type { CreateProjectRequest } from '@/lib/api/types';

export async function GET(
  _request: NextRequest,
  context: { params: Promise<{ projectId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { projectId } = await context.params;
  const detail = await getProjectHydrated(auth.user.user_id, projectId);
  if (!detail) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}

export async function PUT(
  request: NextRequest,
  context: { params: Promise<{ projectId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { projectId } = await context.params;
  const body = (await request.json()) as Partial<CreateProjectRequest>;
  const detail = updateProject(auth.user.user_id, projectId, body);
  if (!detail) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}
