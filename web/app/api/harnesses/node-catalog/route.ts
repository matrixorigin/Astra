import { NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'list harness node catalog',
    });
    return NextResponse.json(await runtime.get('/harnesses/node-catalog'));
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to list harness node catalog.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
