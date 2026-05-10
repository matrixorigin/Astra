import { NextRequest, NextResponse } from 'next/server';
import { createProject, listProjects } from '@/lib/api/web-store';
import type { CreateProjectRequest } from '@/lib/api/types';

export async function GET(request: NextRequest) {
  const params = request.nextUrl.searchParams;
  return NextResponse.json(
    listProjects({
      q: params.get('q'),
      sort: (params.get('sort') as 'activity' | 'created' | 'name' | null) ?? 'activity',
      cursor: params.get('cursor'),
      limit: Number(params.get('limit') ?? 24),
    }),
  );
}

export async function POST(request: NextRequest) {
  const body = (await request.json()) as CreateProjectRequest;
  if (!body.name?.trim()) {
    return NextResponse.json({ error: 'name is required' }, { status: 400 });
  }
  return NextResponse.json({ project: createProject(body) }, { status: 201 });
}
