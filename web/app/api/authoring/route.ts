import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(request: NextRequest) {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'create authoring intent',
    });
    const body = await request.json();
    return NextResponse.json(await runtime.post('/harnesses/authoring', body), { status: 201 });
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to start authoring.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
