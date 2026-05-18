import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(
  request: NextRequest,
  { params }: { params: Promise<{ runId: string; draftId: string }> },
) {
  const { runId, draftId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'publish Skillify draft',
    });
    const body = await request.json();
    return NextResponse.json(
      await runtime.post(
        `/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts/${encodeURIComponent(draftId)}/publish`,
        body,
      ),
      { status: 201 },
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to publish Skillify draft.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
