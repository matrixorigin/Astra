import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeAuth } from '@/lib/api/auth-guard';
import { getChat, sendMessage } from '@/lib/api/web-store';
import type { SendMessageRequest } from '@/lib/api/types';

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const { chatId } = await context.params;
  const body = (await request.json()) as SendMessageRequest;
  if (!body.content?.trim()) {
    return NextResponse.json({ error: 'content is required' }, { status: 400 });
  }
  const chat = getChat(chatId);
  if (!chat) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  if (chat.chat.archivedAt) {
    return NextResponse.json({ error: 'archived chat is read-only' }, { status: 409 });
  }
  const authError = await requireRuntimeAuth();
  if (authError) {
    return authError;
  }
  const result = await sendMessage(chatId, body);
  if (!result) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  return NextResponse.json(result);
}
