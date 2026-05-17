import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET(
  _request: NextRequest,
  { params }: { params: Promise<{ runId: string }> },
) {
  const { runId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'list harness run items',
    });
    return NextResponse.json(await runtime.get(`/harnesses/runs/${encodeURIComponent(runId)}/items`));
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to load harness items.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
