import { NextRequest, NextResponse } from 'next/server';
import { AstraApiError } from '@astra/sdk';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

function failure(error: unknown) {
  return NextResponse.json({ error: runtimeErrorDetail(error, 'Session service unavailable.') }, {
    status: error instanceof RuntimeClientError ? (error.status ?? 502) : error instanceof AstraApiError ? error.status : 502,
  });
}

export async function GET(request: NextRequest) {
  try {
    const runtime = await requireRuntimeClient({ auth: 'required', operation: 'select a Skill session' });
    const updatedAt = request.nextUrl.searchParams.get('after_updated_at');
    const sessionId = request.nextUrl.searchParams.get('after_session_id');
    if (!!updatedAt !== !!sessionId) return NextResponse.json({ error: 'Incomplete session cursor.' }, { status: 400 });
    return NextResponse.json(await runtime.sdk.listRuntimeSessions({ limit: 50,
      ...(updatedAt && sessionId ? { cursor: { updated_at: updatedAt, session_id: sessionId } } : {}),
    }));
  } catch (error) { return failure(error); }
}

export async function POST(request: NextRequest) {
  try {
    const runtime = await requireRuntimeClient({ auth: 'required', operation: 'create a session' });
    const { title } = await request.json();
    if (typeof title !== 'string' || !title.trim()) return NextResponse.json({ error: 'Session title is required.' }, { status: 400 });
    return NextResponse.json(await runtime.sdk.createRuntimeSession({ title, metadata: { source: 'web_v1' } }), { status: 201 });
  } catch (error) { return failure(error); }
}
