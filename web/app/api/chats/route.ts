import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { createChatWithMessage, deleteArchivedChats, listChats } from '@/lib/api/web-store';
import type { CreateChatRequest } from '@/lib/api/types';
import { RuntimeClientError, requireRuntimeClient } from '@/lib/runtime-client';
import { verifyLiveWorkspaceSelection } from '@/lib/workspace-selection-server';
import { normalizeWorkspaceSelection } from '@/lib/workspace-authority';

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
    const normalizedWorkspaceSelection =
      body.workspaceSelection === null
        ? null
        : body.workspaceSelection === undefined
          ? undefined
          : normalizeWorkspaceSelection(body.workspaceSelection);
    if (
      body.workspaceSelection !== undefined &&
      body.workspaceSelection !== null &&
      !normalizedWorkspaceSelection
    ) {
      return NextResponse.json(
        { error: 'workspaceSelection must be a valid environment selection' },
        { status: 400 },
      );
    }
    const workspaceSelection =
      normalizedWorkspaceSelection?.kind === 'edge_workspace'
        ? await verifyLiveWorkspaceSelection(
            normalizedWorkspaceSelection,
            await requireRuntimeClient({
              auth: 'required',
              operation: 'verify initial chat environment selection',
            }),
          )
        : normalizedWorkspaceSelection;
    const result = await createChatWithMessage(auth.user.user_id, {
      message: body.message,
      model: body.model,
      options: body.options,
      projectId: body.projectId,
      workspaceSelection: workspaceSelection ?? undefined,
    });
    return NextResponse.json(result, { status: 201 });
  } catch (error) {
    if (error instanceof RuntimeClientError && error.status) {
      return NextResponse.json({ error: error.detail }, { status: error.status });
    }
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
