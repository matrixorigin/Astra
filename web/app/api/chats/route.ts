import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { createChatWithMessage, deleteArchivedChats, listChats } from '@/lib/api/web-store';
import type { CreateChatRequest } from '@/lib/api/types';

export async function GET(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const params = request.nextUrl.searchParams;
  const projectIdParam = params.get('projectId');
  return NextResponse.json(
    await listChats(auth.user.user_id, {
      projectId: projectIdParam === null ? undefined : projectIdParam,
      q: params.get('q'),
      cursor: params.get('cursor'),
      limit: Number(params.get('limit') ?? 50),
      archived: params.get('archived') === 'true',
    }),
  );
}

export async function POST(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }

  const body = (await request.json()) as CreateChatRequest;
  if (!body.message?.trim()) {
    return NextResponse.json({ error: 'message is required' }, { status: 400 });
  }
  try {
    const result = await createChatWithMessage(auth.user.user_id, {
      message: body.message,
      model: body.model,
      options: body.options,
      projectId: body.projectId,
    });
    return NextResponse.json(result, { status: 201 });
  } catch (error) {
    const message = error instanceof Error ? error.message : 'failed to create chat';
    const status = message.includes('authentication is missing') ? 401 : 502;
    return NextResponse.json({ error: message }, { status });
  }
}

export async function DELETE(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const params = request.nextUrl.searchParams;
  if (params.get('archived') !== 'true') {
    return NextResponse.json({ error: 'archived=true is required' }, { status: 400 });
  }

  try {
    const deleted = await deleteArchivedChats(auth.user.user_id);
    return NextResponse.json({ deleted });
  } catch (error) {
    const message = error instanceof Error ? error.message : 'failed to clear archived chats';
    return NextResponse.json({ error: message }, { status: 502 });
  }
}
