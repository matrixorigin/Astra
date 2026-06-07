import { notFound } from 'next/navigation';
import { ChatView } from '@/components/app/chat-view';
import type { ChatDetail } from '@/lib/api/types';

export const dynamic = 'force-dynamic';

function activeRunForStatus(status: string | null): ChatDetail['activeRun'] {
  if (!status || status === 'idle') {
    return undefined;
  }
  return {
    runId: 'run-e2e',
    status,
    waitingFor: status === 'paused' ? 'user_resume' : null,
  };
}

export default async function E2eChatViewPage({
  searchParams,
}: {
  searchParams: Promise<{ status?: string; long?: string }>;
}) {
  if (process.env.ASTRA_ENABLE_E2E_PAGES !== '1') {
    notFound();
  }

  const { status = 'running', long } = await searchParams;
  const messages = long === '1'
    ? Array.from({ length: 30 }, (_, index) => ({
        id: `message-existing-${index}`,
        role: 'user' as const,
        content: `Existing message ${index + 1}\n\nThis message adds enough height for real browser scroll behavior.`,
        createdAt: '2026-06-07T00:00:00.000Z',
        status: 'complete' as const,
      }))
    : [{
        id: 'message-existing',
        role: 'user' as const,
        content: 'Existing message',
        createdAt: '2026-06-07T00:00:00.000Z',
        status: 'complete' as const,
      }];
  const detail: ChatDetail = {
    chat: {
      id: 'chat-e2e',
      title: 'E2E Chat',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      updatedAt: '2026-06-07T00:00:00.000Z',
      archivedAt: null,
      model: 'sonnet-4.6-adaptive',
    },
    messages,
    activeRun: activeRunForStatus(status),
  };

  return <ChatView initial={detail} />;
}
