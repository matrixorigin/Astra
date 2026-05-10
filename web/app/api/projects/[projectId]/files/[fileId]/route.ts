import { NextResponse } from 'next/server';
import { removeProjectFile } from '@/lib/api/web-store';

export async function DELETE(
  _request: Request,
  context: { params: Promise<{ projectId: string; fileId: string }> },
) {
  const { projectId, fileId } = await context.params;
  if (!removeProjectFile(projectId, fileId)) {
    return NextResponse.json({ error: 'file not found' }, { status: 404 });
  }
  return NextResponse.json({ ok: true });
}
