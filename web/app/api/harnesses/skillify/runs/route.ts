import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(request: NextRequest) {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'create skillify harness run',
    });
    const body = await request.json();
    return NextResponse.json(await runtime.post('/harnesses/skillify/runs', body), { status: 201 });
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to create Skillify harness run.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
