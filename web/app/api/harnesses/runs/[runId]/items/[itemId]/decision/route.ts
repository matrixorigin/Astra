import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(
  request: NextRequest,
  { params }: { params: Promise<{ runId: string; itemId: string }> },
) {
  const { runId, itemId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'decide harness item',
    });
    const body = await request.json();
    return NextResponse.json(
      await runtime.post(
        `/harnesses/runs/${encodeURIComponent(runId)}/items/${encodeURIComponent(itemId)}/decision`,
        body,
      ),
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to update harness item.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
