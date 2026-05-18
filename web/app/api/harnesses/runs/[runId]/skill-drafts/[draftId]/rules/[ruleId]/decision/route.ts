import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(
  request: NextRequest,
  { params }: { params: Promise<{ runId: string; draftId: string; ruleId: string }> },
) {
  const { runId, draftId, ruleId } = await params;
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'decide Skillify rule',
    });
    const body = await request.json();
    return NextResponse.json(
      await runtime.post(
        `/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts/${encodeURIComponent(draftId)}/rules/${encodeURIComponent(ruleId)}/decision`,
        body,
      ),
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to update Skillify rule.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
