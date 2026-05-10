import { NextRequest, NextResponse } from 'next/server';
import { archiveChat, deleteChat, getChat, moveChat } from '@/lib/api/web-store';

export async function GET(
  _request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const { chatId } = await context.params;
  const detail = getChat(chatId);
  if (!detail) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}

export async function PATCH(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const { chatId } = await context.params;
  const body = (await request.json()) as { projectId?: string | null; archived?: boolean };
  let detail = getChat(chatId);
  if (!detail) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }

  if (body.archived !== undefined) {
    try {
      detail = await archiveChat(chatId, body.archived);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'failed to update archive state';
      return NextResponse.json({ error: message }, { status: 502 });
    }
  }

  if (Object.prototype.hasOwnProperty.call(body, 'projectId')) {
    detail = moveChat(chatId, body.projectId ?? null);
  }

  if (!detail) {
    return NextResponse.json({ error: 'chat or project not found' }, { status: 404 });
  }
  return NextResponse.json(detail);
}

export async function DELETE(
  _request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const { chatId } = await context.params;
  try {
    const deleted = await deleteChat(chatId);
    if (!deleted) {
      return NextResponse.json({ error: 'chat not found' }, { status: 404 });
    }
    return NextResponse.json({ deleted: true });
  } catch (error) {
    const message = error instanceof Error ? error.message : 'failed to delete chat';
    return NextResponse.json({ error: message }, { status: 502 });
  }
}
