import { NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET(
  _request: Request,
  { params }: { params: Promise<{ runId: string; draftId: string }> },
) {
  const { runId, draftId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'get Skillify draft',
    });
    return NextResponse.json(
      await runtime.get(
        `/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts/${encodeURIComponent(draftId)}`,
      ),
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to get Skillify draft.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
