import { NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { removeProjectFile } from '@/lib/api/web-store';

export async function DELETE(
  _request: Request,
  context: { params: Promise<{ projectId: string; fileId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { projectId, fileId } = await context.params;
  if (!removeProjectFile(auth.user.user_id, projectId, fileId)) {
    return NextResponse.json({ error: 'file not found' }, { status: 404 });
  }
  return NextResponse.json({ ok: true });
}
