import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import {
  archiveChat,
  deleteChat,
  getChat,
  getChatHydrated,
  moveChat,
  updateChatModel,
  updateChatWorkspaceSelection,
} from '@/lib/api/web-store';
import type { WorkspaceSelection } from '@/lib/api/types';
import { normalizeWorkspaceSelection } from '@/lib/workspace-authority';
import { RuntimeClientError, requireRuntimeClient } from '@/lib/runtime-client';
import { verifyLiveWorkspaceSelection } from '@/lib/workspace-selection-server';

export async function GET(_request: NextRequest, context: { params: Promise<{ chatId: string }> }) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { chatId } = await context.params;
  const detail = await getChatHydrated(auth.user.user_id, chatId);
  if (!detail) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}

export async function PATCH(request: NextRequest, context: { params: Promise<{ chatId: string }> }) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { chatId } = await context.params;
  const body = (await request.json()) as {
    projectId?: string | null;
    archived?: boolean;
    model?: string;
    workspaceSelection?: WorkspaceSelection | null;
  };
  let detail = getChat(auth.user.user_id, chatId);
  if (!detail) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }

  if (body.archived !== undefined) {
    try {
      detail = await archiveChat(auth.user.user_id, chatId, body.archived);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'failed to update archive state';
      return NextResponse.json({ error: message }, { status: 502 });
    }
  }

  if (Object.prototype.hasOwnProperty.call(body, 'projectId')) {
    detail = moveChat(auth.user.user_id, chatId, body.projectId ?? null);
  }

  if (body.model !== undefined) {
    try {
      detail = await updateChatModel(auth.user.user_id, chatId, body.model);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'failed to update chat model';
      return NextResponse.json({ error: message }, { status: 502 });
    }
  }

  if (Object.prototype.hasOwnProperty.call(body, 'workspaceSelection')) {
    let workspaceSelection: WorkspaceSelection | null;
    if (body.workspaceSelection === null) {
      workspaceSelection = null;
    } else {
      const normalized = normalizeWorkspaceSelection(body.workspaceSelection);
      if (!normalized) {
        return NextResponse.json(
          { error: 'workspaceSelection must be a valid environment selection' },
          { status: 400 },
        );
      }
      workspaceSelection = normalized;
    }
    try {
      if (workspaceSelection?.kind === 'edge_workspace') {
        const runtime = await requireRuntimeClient({
          auth: 'required',
          operation: 'verify chat environment selection',
        });
        workspaceSelection =
          (await verifyLiveWorkspaceSelection(workspaceSelection, runtime)) ??
          null;
      }
      detail = await updateChatWorkspaceSelection(auth.user.user_id, chatId, workspaceSelection);
    } catch (error) {
      if (error instanceof RuntimeClientError && error.status) {
        return NextResponse.json({ error: error.detail }, { status: error.status });
      }
      const message = error instanceof Error ? error.message : 'failed to update chat environment';
      return NextResponse.json({ error: message }, { status: 502 });
    }
  }

  if (!detail) {
    return NextResponse.json({ error: 'chat or project not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}

export async function DELETE(_request: NextRequest, context: { params: Promise<{ chatId: string }> }) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { chatId } = await context.params;
  const detail = getChat(auth.user.user_id, chatId);
  if (!detail) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  try {
    const deleted = await deleteChat(auth.user.user_id, chatId);
    if (!deleted) {
      return NextResponse.json({ error: 'chat not found' }, { status: 404 });
    }
    return NextResponse.json({ deleted: true });
  } catch (error) {
    const message = error instanceof Error ? error.message : 'failed to delete chat';
    return NextResponse.json({ error: message }, { status: 502 });
  }
}
