import { NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'list harness templates',
    });
    return NextResponse.json(await runtime.get('/harnesses/templates'));
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to list harness templates.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
