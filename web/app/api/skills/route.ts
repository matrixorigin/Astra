import { NextRequest, NextResponse } from 'next/server';
import { AstraApiError, type RuntimeSkillListCursor } from '@astra/sdk';
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

function cursorParam(params: URLSearchParams): RuntimeSkillListCursor | undefined {
  const skillName = params.get('after_skill_name');
  const version = params.get('after_version');
  const skillId = params.get('after_skill_id');
  const hasAny = skillName !== null || version !== null || skillId !== null;
  if (!hasAny) {
    return undefined;
  }
  if (!skillName || !version || !skillId) {
    throw new Error('Skill cursor requires after_skill_name, after_version, and after_skill_id.');
  }
  return {
    skill_name: skillName,
    version,
    skill_id: skillId,
  };
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
      cursor: cursorParam(params),
    });
    return NextResponse.json(payload);
  } catch (error) {
    if (error instanceof Error && error.message.startsWith('Skill cursor requires ')) {
      return NextResponse.json({ error: error.message }, { status: 400 });
    }
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
