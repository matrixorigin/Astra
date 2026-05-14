import { NextRequest, NextResponse } from 'next/server';
import { AstraApiError } from '@astra/sdk';
import { RuntimeClientError, runtimeErrorDetail, requireRuntimeClient } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

function intParam(value: string | null, fallback: number) {
  if (value === null) {
    return fallback;
  }
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return fallback;
  }
  return Math.trunc(parsed);
}

export async function GET(request: NextRequest) {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'list runtime skills',
    });
    const params = request.nextUrl.searchParams;
    const payload = await runtime.sdk.listRuntimeSkills({
      limit: intParam(params.get('limit'), 100),
      offset: intParam(params.get('offset'), 0),
    });
    return NextResponse.json(payload);
  } catch (error) {
    const status = error instanceof RuntimeClientError
      ? (error.status ?? 502)
      : error instanceof AstraApiError
        ? error.status
        : 502;
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to list runtime skills.') },
      { status },
    );
  }
}
