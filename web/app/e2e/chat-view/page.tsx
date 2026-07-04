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
  searchParams: Promise<{ status?: string; long?: string; reasoning?: string }>;
}) {
  if (process.env.ASTRA_ENABLE_E2E_PAGES !== '1') {
    notFound();
  }

  const { status = 'running', long, reasoning } = await searchParams;
  const baseMessages = long === '1'
    ? Array.from({ length: 48 }, (_, index) => ({
        id: `message-existing-${index}`,
        role: 'user' as const,
        content: `Existing message ${index + 1}

This message adds enough height for real browser scroll behavior.
It intentionally spans multiple lines so scroll tests do not depend on viewport size.
The chat should remain readable while the assistant is still thinking.
Manual scrollback must not be pulled back to the newest message.`,
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
  const messages = reasoning
    ? [
        ...baseMessages,
        {
          id: 'message-reasoning',
          role: 'assistant' as const,
          content:
            reasoning === 'streaming'
              ? ''
              : 'The connection path is now simplified.',
          reasoning:
            'Checking the runtime boundary and pruning environment controls from the main chat surface.',
          reasoningStatus:
            reasoning === 'streaming'
              ? ('streaming' as const)
              : ('complete' as const),
          createdAt: new Date(Date.now() - 20_000).toISOString(),
          completedAt:
            reasoning === 'streaming' || reasoning === 'segmentdone'
              ? null
              : new Date().toISOString(),
          status:
            reasoning === 'streaming' || reasoning === 'segmentdone'
              ? ('streaming' as const)
              : ('complete' as const),
        },
      ]
    : baseMessages;
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

  return (
    <main className="h-screen">
      <ChatView initial={detail} />
    </main>
  );
}
