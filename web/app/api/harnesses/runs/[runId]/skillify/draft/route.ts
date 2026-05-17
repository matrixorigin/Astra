import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(
  request: NextRequest,
  { params }: { params: Promise<{ runId: string }> },
) {
  const { runId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'create Skillify draft',
    });
    const body = await request.json();
    return NextResponse.json(
      await runtime.post(`/harnesses/runs/${encodeURIComponent(runId)}/skillify/draft`, body),
      { status: 201 },
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to create Skillify draft.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
