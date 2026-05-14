import { notFound } from 'next/navigation';
import { ChatView } from '@/components/app/chat-view';
import { getCurrentUser } from '@/lib/auth/actions';
import { getChatHydrated } from '@/lib/api/web-store';

export default async function ProjectChatPage({
  params,
}: {
  params: Promise<{ projectId: string; chatId: string }>;
}) {
  const user = await getCurrentUser();
  if (!user) {
    notFound();
  }
  const { projectId, chatId } = await params;
  const detail = await getChatHydrated(user.user_id, chatId);
  if (!detail || detail.chat.projectId !== projectId) {
    notFound();
  }
  return <ChatView initial={detail} />;
}
