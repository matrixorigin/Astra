import { NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { setProjectStar } from '@/lib/api/web-store';

export async function POST(
  _request: Request,
  context: { params: Promise<{ projectId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { projectId } = await context.params;
  const result = setProjectStar(auth.user.user_id, projectId, true);
  if (!result) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json(result);
}

export async function DELETE(
  _request: Request,
  context: { params: Promise<{ projectId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { projectId } = await context.params;
  const result = setProjectStar(auth.user.user_id, projectId, false);
  if (!result) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json(result);
}
