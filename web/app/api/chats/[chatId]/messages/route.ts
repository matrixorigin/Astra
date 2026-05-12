import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { getChatHydrated, sendMessage } from '@/lib/api/web-store';
import type { SendMessageRequest } from '@/lib/api/types';

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { chatId } = await context.params;
  const body = (await request.json()) as SendMessageRequest;
  if (!body.content?.trim()) {
    return NextResponse.json({ error: 'content is required' }, { status: 400 });
  }
  const chat = await getChatHydrated(auth.user.user_id, chatId);
  if (!chat) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  if (chat.chat.archivedAt) {
    return NextResponse.json({ error: 'archived chat is read-only' }, { status: 409 });
  }
  const result = await sendMessage(auth.user.user_id, chatId, body);
  if (!result) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  return NextResponse.json(result);
}
