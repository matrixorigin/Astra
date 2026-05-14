import { notFound } from 'next/navigation';
import { ChatView } from '@/components/app/chat-view';
import { getCurrentUser } from '@/lib/auth/actions';
import { getChatHydrated } from '@/lib/api/web-store';

export default async function ChatPage({
  params,
}: {
  params: Promise<{ chatId: string }>;
}) {
  const user = await getCurrentUser();
  if (!user) {
    notFound();
  }
  const { chatId } = await params;
  const detail = await getChatHydrated(user.user_id, chatId);
  if (!detail) {
    notFound();
  }
  return <ChatView initial={detail} />;
}
