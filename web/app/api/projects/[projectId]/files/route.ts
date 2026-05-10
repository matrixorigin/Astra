import { NextRequest, NextResponse } from 'next/server';
import { addProjectFile } from '@/lib/api/web-store';

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ projectId: string }> },
) {
  const { projectId } = await context.params;
  const form = await request.formData();
  const file = form.get('file');
  if (!(file instanceof File)) {
    return NextResponse.json({ error: 'file is required' }, { status: 400 });
  }
  const record = addProjectFile(projectId, file);
  if (!record) {
    return NextResponse.json({ error: 'project not found' }, { status: 404 });
  }
  return NextResponse.json({ file: record }, { status: 201 });
}
