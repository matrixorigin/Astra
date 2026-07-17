import { PATH_RUNTIME_CAPABILITIES } from '@astra/sdk';
import { NextResponse } from 'next/server';
import type { RuntimeCapabilitiesResponse } from '@/lib/api/types';
import { requireRuntimeClient } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function GET() {
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
}
