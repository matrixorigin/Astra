import { notFound } from 'next/navigation';
import { ChatView } from '@/components/app/chat-view';
import { getChat } from '@/lib/api/web-store';

export default async function ChatPage({
  params,
}: {
  params: Promise<{ chatId: string }>;
}) {
  const { chatId } = await params;
  const detail = getChat(chatId);
  if (!detail) {
    notFound();
  }
  return <ChatView initial={detail} />;
}
