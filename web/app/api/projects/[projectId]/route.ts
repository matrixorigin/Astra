import { NextRequest, NextResponse } from 'next/server';
import { getProject, updateProject } from '@/lib/api/web-store';
import type { CreateProjectRequest } from '@/lib/api/types';

export async function GET(
  _request: NextRequest,
  context: { params: Promise<{ projectId: string }> },
) {
  const { projectId } = await context.params;
  const detail = getProject(projectId);
  if (!detail) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}

export async function PUT(
  request: NextRequest,
  context: { params: Promise<{ projectId: string }> },
) {
  const { projectId } = await context.params;
  const body = (await request.json()) as Partial<CreateProjectRequest>;
  const detail = updateProject(projectId, body);
  if (!detail) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}
