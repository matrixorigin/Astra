import { NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET(
  _request: Request,
  { params }: { params: Promise<{ runId: string }> },
) {
  const { runId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'list Skillify drafts',
    });
    return NextResponse.json(await runtime.get(`/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts`));
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to list Skillify drafts.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
