import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { createProject, listProjects } from '@/lib/api/web-store';
import type { CreateProjectRequest } from '@/lib/api/types';

export async function GET(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const params = request.nextUrl.searchParams;
  return NextResponse.json(
    listProjects(auth.user.user_id, {
      q: params.get('q'),
      sort: (params.get('sort') as 'activity' | 'created' | 'name' | null) ?? 'activity',
      cursor: params.get('cursor'),
      limit: Number(params.get('limit') ?? 24),
    }),
  );
}

export async function POST(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const body = (await request.json()) as CreateProjectRequest;
  if (!body.name?.trim()) {
    return NextResponse.json({ error: 'name is required' }, { status: 400 });
  }
  return NextResponse.json({ project: createProject(auth.user.user_id, body) }, { status: 201 });
}
