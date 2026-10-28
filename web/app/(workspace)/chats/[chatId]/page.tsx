import { notFound } from 'next/navigation';
import { ChatView } from '@/components/app/chat-view';
import { currentUserContext } from '@/lib/auth/current-user';
import { getChatHydrated } from '@/lib/api/web-store';

export default async function ChatPage({ params }: { params: Promise<{ chatId: string }> }) {
  const auth = await currentUserContext();
  if (!auth) {
    notFound();
  }
  const { chatId } = await params;
  const detail = await getChatHydrated(auth.user.user_id, chatId, auth.client);
  if (!detail) {
    notFound();
  }
  return <ChatView initial={detail} />;
}
