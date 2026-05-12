import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { addProjectFile } from '@/lib/api/web-store';

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ projectId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { projectId } = await context.params;
  const form = await request.formData();
  const file = form.get('file');
  if (!(file instanceof File)) {
    return NextResponse.json({ error: 'file is required' }, { status: 400 });
  }
  const record = addProjectFile(auth.user.user_id, projectId, file);
  if (!record) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json({ file: record }, { status: 201 });
}
