import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export async function POST(request: NextRequest, { params }: { params: Promise<{ skillName: string }> }) {
  const { skillName } = await params;
  try {
    const runtime = await requireRuntimeClient({ auth: 'required', operation: 'activate personal Skill' });
    return NextResponse.json(await runtime.post(`/skills/user/${encodeURIComponent(skillName)}/activate`, await request.json()));
  } catch (error) {
    return NextResponse.json({ error: runtimeErrorDetail(error, 'Failed to use this Skill revision.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 });
  }
}
