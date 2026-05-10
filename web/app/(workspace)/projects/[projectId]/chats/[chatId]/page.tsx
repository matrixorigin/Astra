import { notFound } from 'next/navigation';
import { ChatView } from '@/components/app/chat-view';
import { getChat } from '@/lib/api/web-store';

export default async function ProjectChatPage({
  params,
}: {
  params: Promise<{ projectId: string; chatId: string }>;
}) {
  const { projectId, chatId } = await params;
  const detail = getChat(chatId);
  if (!detail || detail.chat.projectId !== projectId) {
    notFound();
  }
  return <ChatView initial={detail} />;
}
