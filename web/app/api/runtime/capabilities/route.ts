import { PATH_RUNTIME_CAPABILITIES } from '@astra/sdk';
import { NextResponse } from 'next/server';
import type { RuntimeCapabilitiesResponse } from '@/lib/api/types';
import {
  RuntimeClientError,
  requireRuntimeClient,
  runtimeErrorDetail,
} from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'discover runtime capabilities',
    });
    return NextResponse.json(
      await runtime.get<RuntimeCapabilitiesResponse>(PATH_RUNTIME_CAPABILITIES, {
        auth: 'required',
        operation: 'discover runtime capabilities',
      }),
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error) },
      {
        status:
          error instanceof RuntimeClientError && error.status
            ? error.status
            : 502,
      },
    );
  }
}
