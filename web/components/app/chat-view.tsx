'use client';

import { MoreVertical } from 'lucide-react';
import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useCallback, useEffect, useRef, useState } from 'react';
import { ChatActionsMenu } from '@/components/app/chat-actions-menu';
import { Composer } from '@/components/app/composer';
import { MessageBubble } from '@/components/app/message-bubble';
import { MoveChatModal } from '@/components/app/move-chat-modal';
import { IconButton } from '@/components/ui/icon-button';
import { useChatLifecycleActions } from '@/hooks/use-chat-lifecycle-actions';
import { subscribeChatLifecycleChange } from '@/lib/chat-lifecycle-events';
import { getChat, streamChatMessage } from '@/lib/api/chats';
import { isAuthRequiredError } from '@/lib/api/errors';
import type { ChatDetail, ChatMessage, ComposerOptions } from '@/lib/api/types';

export function ChatView({ initial }: { initial: ChatDetail }) {
  const router = useRouter();
  const [detail, setDetail] = useState(initial);
  const [moveOpen, setMoveOpen] = useState(false);
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const pinnedRef = useRef(true);
  const pendingStartedRef = useRef<string | null>(null);
  const lifecycle = useChatLifecycleActions({ onChatUpdated: setDetail });

  const latestMessage = detail.messages[detail.messages.length - 1];
  const isArchived = Boolean(detail.chat.archivedAt);
  const chatListHref = detail.chat.projectId ? `/projects/${detail.chat.projectId}` : '/chats';

  const startStream = useCallback(async ({
    text,
    options,
    pendingMessageId,
    appendUser,
  }: {
    text: string;
    options: ComposerOptions;
    pendingMessageId?: string;
    appendUser: boolean;
  }) => {
    if (detail.chat.archivedAt) {
      return;
    }

    const timestamp = new Date().toISOString();
    const assistantId = crypto.randomUUID();
    const userMessage: ChatMessage = {
      id: crypto.randomUUID(),
      role: 'user',
      content: text,
      createdAt: timestamp,
      status: 'complete',
    };
    const assistantMessage: ChatMessage = {
      id: assistantId,
      role: 'assistant',
      content: '',
      createdAt: timestamp,
      status: 'streaming',
      reasoning: '',
      reasoningStatus: 'streaming',
    };

    const patchAssistant = (patch: Partial<ChatMessage>) => {
      setDetail((current) => ({
        ...current,
        messages: current.messages.map((message) => (
          message.id === assistantId ? { ...message, ...patch } : message
        )),
      }));
    };

    setSending(true);
    setDetail((current) => ({
      ...current,
      pendingTurn: pendingMessageId ? undefined : current.pendingTurn,
      messages: appendUser
        ? [...current.messages, userMessage, assistantMessage]
        : [...current.messages, assistantMessage],
    }));
    try {
      await streamChatMessage(detail.chat.id, {
        content: text,
        options,
        pendingMessageId,
      }, {
        onReasoning: (reasoning) => {
          patchAssistant({ reasoning, reasoningStatus: 'streaming', status: 'streaming' });
        },
        onReasoningDone: (reasoning) => {
          patchAssistant({ reasoning, reasoningStatus: 'complete', status: 'streaming' });
        },
        onText: (content) => {
          patchAssistant({ content, status: 'streaming' });
        },
        onDone: (content) => {
          patchAssistant({
            content: content || 'Astra completed the run without returning visible text.',
            reasoningStatus: 'complete',
            status: 'complete',
          });
        },
      });
    } catch (error) {
      if (isAuthRequiredError(error)) {
        router.push(`/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`);
        return;
      }
      const message = error instanceof Error ? error.message : 'Astra stream failed.';
      patchAssistant({
        content: `I could not reach the Astra runtime from the web UI. (${message})`,
        status: 'failed',
      });
    } finally {
      setSending(false);
    }
  }, [detail.chat.archivedAt, detail.chat.id, router]);

  useEffect(() => {
    if (pinnedRef.current) {
      scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
    }
  }, [detail.messages.length, latestMessage?.content, latestMessage?.reasoning]);

  useEffect(() => {
    if (
      isArchived ||
      !detail.pendingTurn ||
      pendingStartedRef.current === detail.pendingTurn.messageId
    ) {
      return;
    }
    pendingStartedRef.current = detail.pendingTurn.messageId;
    void startStream({
      text: detail.pendingTurn.content,
      options: detail.pendingTurn.options,
      pendingMessageId: detail.pendingTurn.messageId,
      appendUser: false,
    });
  }, [detail.pendingTurn, isArchived, startStream]);

  async function refresh() {
    setDetail(await getChat(detail.chat.id));
  }

  useEffect(() => subscribeChatLifecycleChange((event) => {
    if (event.action === 'clearArchived') {
      if (isArchived) {
        router.replace(chatListHref);
      }
      return;
    }
    if (event.chatId !== detail.chat.id) {
      return;
    }
    if (event.action === 'delete') {
      router.replace(chatListHref);
      return;
    }
    if (event.chat) {
      setDetail(event.chat);
      return;
    }
    void getChat(detail.chat.id)
      .then(setDetail)
      .catch((error: unknown) => {
        window.alert(error instanceof Error ? error.message : 'Failed to refresh chat state.');
      });
  }), [chatListHref, detail.chat.id, isArchived, router]);

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden">
      <header className="flex min-h-16 shrink-0 items-center gap-4 border-b border-border px-8">
        <Link
          href={detail.chat.projectId ? `/projects/${detail.chat.projectId}` : '/chats'}
          className="text-sm text-text-secondary hover:text-text"
        >
          ← {detail.project?.name ?? 'Chats'}
        </Link>
        <h1 className="min-w-0 flex-1 truncate text-base font-semibold">{detail.chat.title ?? 'Untitled'}</h1>
        {detail.chat.archivedAt ? (
          <span className="rounded-full bg-surface-muted px-2 py-1 text-xs font-medium text-text-muted">
            Archived
          </span>
        ) : null}
        <ChatActionsMenu
          chatId={detail.chat.id}
          archived={isArchived}
          active
          afterMutationHref={chatListHref}
          onMove={() => setMoveOpen(true)}
          onChatUpdated={setDetail}
          trigger={<IconButton icon={MoreVertical} label="Chat menu" />}
        />
      </header>

      <div
        ref={scrollRef}
        onScroll={(event) => {
          const target = event.currentTarget;
          pinnedRef.current = target.scrollHeight - target.scrollTop - target.clientHeight < 80;
        }}
        className="min-h-0 flex-1 overscroll-contain overflow-y-auto scroll-smooth"
      >
        <div className="mx-auto w-full max-w-composer px-6 py-4 pb-8">
          {detail.messages.map((message) => <MessageBubble key={message.id} message={message} />)}
        </div>
      </div>

      <div className="shrink-0 bg-bg px-4 pb-3 pt-4 md:px-6">
        <div className="mx-auto max-w-composer">
          {isArchived ? (
            <div className="rounded-[20px] border border-border bg-surface px-5 py-4 shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.06),0_0_0_0.5px_rgba(120,113,108,0.18)]">
              <div className="flex items-center justify-between gap-4">
                <div className="min-w-0">
                  <p className="text-sm font-medium text-text">This chat is archived.</p>
                  <p className="mt-1 text-sm text-text-muted">Archived chats are read-only. Unarchive it to continue.</p>
                </div>
                <button
                  type="button"
                  disabled={lifecycle.busyChatId === detail.chat.id}
                  onClick={() => { void lifecycle.unarchive(detail.chat.id); }}
                  className="shrink-0 rounded-control bg-text px-3 py-2 text-sm font-medium text-white hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Unarchive
                </button>
              </div>
            </div>
          ) : (
            <Composer
              disabled={sending}
              placeholder="Reply to Astra..."
              initialModel={detail.pendingTurn?.options.model}
              projectContext={detail.chat.projectId ? { projectId: detail.chat.projectId } : undefined}
              onSubmit={async ({ text, options }) => {
                await startStream({ text, options, appendUser: true });
              }}
            />
          )}
        </div>
      </div>

      <MoveChatModal
        open={moveOpen}
        chatId={detail.chat.id}
        currentProjectId={detail.chat.projectId}
        onOpenChange={setMoveOpen}
        onMoved={refresh}
      />
    </div>
  );
}
