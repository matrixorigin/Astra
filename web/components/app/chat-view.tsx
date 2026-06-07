'use client';

import { MoreVertical } from 'lucide-react';
import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useCallback, useEffect, useRef, useState } from 'react';
import { ChatActionsMenu } from '@/components/app/chat-actions-menu';
import { ChatDotNavigator } from '@/components/app/chat-dot-navigator';
import { Composer } from '@/components/app/composer';
import { MessageBubble } from '@/components/app/message-bubble';
import { MoveChatModal } from '@/components/app/move-chat-modal';
import { IconButton } from '@/components/ui/icon-button';
import { useChatLifecycleActions } from '@/hooks/use-chat-lifecycle-actions';
import { subscribeChatLifecycleChange } from '@/lib/chat-lifecycle-events';
import { getChat, queueChatRunInput, resumeChatRun, stopChatRun, streamChatMessage, streamExistingChatRun, updateChatModel } from '@/lib/api/chats';
import { WebApiError, isAuthRequiredError, isNotFoundError } from '@/lib/api/errors';
import type { ChatDetail, ChatMessage, ComposerOptions } from '@/lib/api/types';
import { deriveChatRunUiState } from '@/lib/chat-run-state';
import { isChatScrolledToBottom, shouldAutoScrollChat } from '@/lib/chat-scroll-state';

function isAbortError(error: unknown) {
  return error instanceof DOMException && error.name === 'AbortError';
}

export function ChatView({ initial }: { initial: ChatDetail }) {
  const router = useRouter();
  const [detail, setDetail] = useState(initial);
  const [moveOpen, setMoveOpen] = useState(false);
  const [startingRun, setStartingRun] = useState(false);
  const [queueingDeferredInput, setQueueingDeferredInput] = useState(false);
  const [resumingRun, setResumingRun] = useState(false);
  const [stoppingRun, setStoppingRun] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const pinnedRef = useRef(true);
  const pendingStartedRef = useRef<string | null>(null);
  const streamAbortRef = useRef<AbortController | null>(null);
  const runControlMutationRef = useRef(false);
  const lifecycle = useChatLifecycleActions({ onChatUpdated: setDetail });

  const latestMessage = detail.messages[detail.messages.length - 1];
  const isArchived = Boolean(detail.chat.archivedAt);
  const chatListHref = detail.chat.projectId ? `/projects/${detail.chat.projectId}` : '/chats';
  const {
    activeRunStatus,
    canQueueDeferredInput,
    canResumeRun,
    canStopRun,
    activeRunBlocksNewInput,
    runControlBusy,
    composerDisabled,
    composerPlaceholder,
    activeRunLabel,
  } = deriveChatRunUiState({
    activeRun: detail.activeRun,
    archived: isArchived,
    startingRun,
    queueingDeferredInput,
    resumingRun,
    stoppingRun,
  });

  const nextStreamAbortSignal = useCallback(() => {
    streamAbortRef.current?.abort();
    const controller = new AbortController();
    streamAbortRef.current = controller;
    return controller.signal;
  }, []);

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
      activeSkills: options.activeSkills,
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

    setStartingRun(true);
    setDetail((current) => ({
      ...current,
      chat: {
        ...current.chat,
        model: options.model,
      },
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
        signal: nextStreamAbortSignal(),
        onRunStarted: (runId) => {
          setStartingRun(false);
          setDetail((current) => ({
            ...current,
            activeRun: {
              runId,
              status: 'running',
              waitingFor: null,
            },
          }));
        },
        onRunUpdated: (run) => {
          setDetail((current) => ({
            ...current,
            activeRun: {
              runId: run.runId,
              status: run.status,
              waitingFor: run.waitingFor ?? null,
            },
          }));
        },
        onRunFinished: () => {
          setStoppingRun(false);
          setStartingRun(false);
          setDetail((current) => ({
            ...current,
            activeRun: undefined,
          }));
        },
        onReasoning: (reasoning) => {
          patchAssistant({ reasoning, reasoningStatus: 'streaming', status: 'streaming' });
        },
        onReasoningDone: (reasoning) => {
          patchAssistant({ reasoning, reasoningStatus: 'complete', status: 'streaming' });
        },
        onText: (content) => {
          patchAssistant({ content, status: 'streaming' });
        },
        onArtifacts: (artifacts) => {
          patchAssistant({ artifacts });
        },
        onDone: (content) => {
          patchAssistant({
            content: content || 'Astra completed the run without returning visible text.',
            reasoningStatus: 'complete',
            status: 'complete',
          });
        },
        onCancelled: (content) => {
          patchAssistant({
            content: content || 'Stopped.',
            reasoningStatus: 'complete',
            status: 'complete',
          });
        },
        onPaused: (content) => {
          patchAssistant({
            content,
            status: 'streaming',
          });
        },
      });
    } catch (error) {
      if (isAbortError(error)) {
        return;
      }
      if (isAuthRequiredError(error)) {
        router.push(`/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`);
        return;
      }
      if (isNotFoundError(error)) {
        setDetail((current) => ({
          ...current,
          messages: current.messages.filter((message) => (
            message.id !== assistantId && (!appendUser || message.id !== userMessage.id)
          )),
        }));
        router.replace(chatListHref);
        return;
      }
      const message = error instanceof Error ? error.message : 'Astra stream failed.';
      patchAssistant({
        content: `I could not reach the Astra runtime from the web UI. (${message})`,
        status: 'failed',
      });
    } finally {
      setStartingRun(false);
    }
  }, [chatListHref, detail.chat.archivedAt, detail.chat.id, nextStreamAbortSignal, router]);

  const queueDeferredInput = useCallback(async ({
    text,
    options,
  }: {
    text: string;
    options: ComposerOptions;
  }) => {
    if (detail.chat.archivedAt) {
      return;
    }
    if (runControlMutationRef.current) {
      return;
    }
    runControlMutationRef.current = true;
    setQueueingDeferredInput(true);

    try {
      const result = await queueChatRunInput(detail.chat.id, {
        content: text,
        options,
      });
      setDetail((current) => ({
        ...current,
        activeRun: result.activeRun,
        messages: [...current.messages, result.userMessage],
      }));
    } catch (error) {
      if (isAuthRequiredError(error)) {
        router.push(`/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`);
        return;
      }
      if (isNotFoundError(error)) {
        router.replace(chatListHref);
        return;
      }
      if (error instanceof WebApiError && error.status === 409) {
        const refreshed = await getChat(detail.chat.id).catch(() => null);
        if (refreshed) {
          setDetail(refreshed);
          if (!refreshed.chat.archivedAt && !refreshed.activeRun?.runId) {
            try {
              await startStream({ text, options, appendUser: true });
              return;
            } catch (fallbackError) {
              window.alert(fallbackError instanceof Error ? fallbackError.message : 'Failed to start a new run.');
              return;
            }
          }
        }
      }
      window.alert(error instanceof Error ? error.message : 'Failed to queue input for the active run.');
    } finally {
      runControlMutationRef.current = false;
      setQueueingDeferredInput(false);
    }
  }, [chatListHref, detail.chat.archivedAt, detail.chat.id, router, startStream]);

  const stopActiveRun = useCallback(async () => {
    if (!detail.activeRun?.runId || !canStopRun) {
      return;
    }
    if (runControlMutationRef.current) {
      return;
    }
    runControlMutationRef.current = true;
    setStoppingRun(true);
    try {
      const result = await stopChatRun(detail.chat.id);
      setDetail((current) => ({
        ...current,
        activeRun: result.activeRun,
      }));
    } catch (error) {
      if (isAuthRequiredError(error)) {
        router.push(`/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`);
        return;
      }
      if (isNotFoundError(error)) {
        router.replace(chatListHref);
        return;
      }
      window.alert(error instanceof Error ? error.message : 'Failed to stop the active run.');
    } finally {
      runControlMutationRef.current = false;
      setStoppingRun(false);
    }
  }, [canStopRun, chatListHref, detail.activeRun?.runId, detail.chat.id, router]);

  const resumeActiveRun = useCallback(async () => {
    if (!detail.activeRun?.runId || !canResumeRun) {
      return;
    }
    if (runControlMutationRef.current) {
      return;
    }
    runControlMutationRef.current = true;
    const existingAssistantMessageId = [...detail.messages]
      .reverse()
      .find((message) => (
        message.role === 'assistant'
        && (message.status === 'streaming' || message.reasoningStatus === 'streaming')
      ))?.id;
    const assistantMessageId = existingAssistantMessageId ?? crypto.randomUUID();
    setResumingRun(true);
    try {
      const result = await resumeChatRun(detail.chat.id);
      setDetail((current) => ({
        ...current,
        activeRun: result.activeRun,
        messages: existingAssistantMessageId || current.messages.some((message) => message.id === assistantMessageId)
          ? current.messages
          : [
              ...current.messages,
              {
                id: assistantMessageId,
                role: 'assistant',
                content: '',
                createdAt: new Date().toISOString(),
                status: 'streaming',
                reasoning: '',
                reasoningStatus: 'streaming',
              },
            ],
      }));
      const patchAssistant = (patch: Partial<ChatMessage>) => {
        setDetail((current) => ({
          ...current,
          messages: current.messages.map((message) => (
            message.id === assistantMessageId ? { ...message, ...patch } : message
          )),
        }));
      };
      try {
        await streamExistingChatRun(detail.chat.id, result.activeRun.runId, {
          signal: nextStreamAbortSignal(),
          onRunUpdated: (run) => {
            setDetail((current) => ({
              ...current,
              activeRun: {
                runId: run.runId,
                status: run.status,
                waitingFor: run.waitingFor ?? null,
              },
            }));
          },
          onRunFinished: () => {
            setStoppingRun(false);
            setDetail((current) => ({
              ...current,
              activeRun: undefined,
            }));
          },
          onReasoning: (reasoning) => {
            patchAssistant({ reasoning, reasoningStatus: 'streaming', status: 'streaming' });
          },
          onReasoningDone: (reasoning) => {
            patchAssistant({ reasoning, reasoningStatus: 'complete', status: 'streaming' });
          },
          onText: (content) => {
            patchAssistant({ content, status: 'streaming' });
          },
          onArtifacts: (artifacts) => {
            patchAssistant({ artifacts });
          },
          onDone: (content) => {
            patchAssistant({
              content: content || 'Astra completed the run without returning visible text.',
              reasoningStatus: 'complete',
              status: 'complete',
            });
          },
          onCancelled: (content) => {
            patchAssistant({
              content: content || 'Stopped.',
              reasoningStatus: 'complete',
              status: 'complete',
            });
          },
          onPaused: (content) => {
            patchAssistant({
              content,
              status: 'streaming',
            });
          },
        });
      } catch (streamError) {
        if (isAbortError(streamError)) {
          return;
        }
        if (isAuthRequiredError(streamError)) {
          router.push(`/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`);
          return;
        }
        if (isNotFoundError(streamError)) {
          router.replace(chatListHref);
          return;
        }
        try {
          const refreshed = await getChat(detail.chat.id);
          setDetail(refreshed);
        } catch {
          // Keep the local running state if refresh also fails; the alert still
          // tells the user that stream reconnection did not attach.
        }
        window.alert(streamError instanceof Error
          ? `The run resumed, but the web UI could not reconnect to its stream. (${streamError.message})`
          : 'The run resumed, but the web UI could not reconnect to its stream.');
      }
    } catch (error) {
      if (isAbortError(error)) {
        return;
      }
      if (isAuthRequiredError(error)) {
        router.push(`/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`);
        return;
      }
      if (isNotFoundError(error)) {
        router.replace(chatListHref);
        return;
      }
      window.alert(error instanceof Error ? error.message : 'Failed to resume the paused run.');
    } finally {
      runControlMutationRef.current = false;
      setResumingRun(false);
    }
  }, [canResumeRun, chatListHref, detail.activeRun?.runId, detail.chat.id, detail.messages, nextStreamAbortSignal, router]);

  useEffect(() => () => {
    streamAbortRef.current?.abort();
  }, []);

  useEffect(() => {
    if (shouldAutoScrollChat({ pinnedToBottom: pinnedRef.current })) {
      scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
    }
  }, [detail.messages.length, latestMessage?.content, latestMessage?.reasoning, latestMessage?.artifacts?.length]);

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

  const handleModelChange = useCallback((model: string) => {
    const previousModel = detail.chat.model ?? null;
    setDetail((current) => ({
      ...current,
      chat: {
        ...current.chat,
        model,
      },
    }));
    void updateChatModel(detail.chat.id, model).catch(() => {
      setDetail((current) => ({
        ...current,
        chat: {
          ...current.chat,
          model: previousModel,
        },
      }));
    });
  }, [detail.chat.id, detail.chat.model]);

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
    <div className="astra-chat-view relative flex h-full min-h-0 flex-col overflow-hidden bg-bg">
      <header className="relative z-10 flex min-h-[58px] shrink-0 items-center gap-4 border-b border-border/60 bg-bg/85 px-7 backdrop-blur">
        <Link
          href={detail.chat.projectId ? `/projects/${detail.chat.projectId}` : '/chats'}
          className="inline-flex items-center gap-1 text-[13px] text-text-muted transition-colors hover:text-text"
        >
          ← {detail.project?.name ?? 'Chats'}
        </Link>
        <div className="min-w-0">
          <h1 className="truncate text-sm font-semibold tracking-[-0.01em]">{detail.chat.title ?? 'Untitled'}</h1>
          <div className="mt-0.5 flex items-center gap-1.5 text-xs text-text-muted">
            <span className="size-1.5 rounded-full bg-success" />
            <span>{activeRunLabel} · {detail.chat.model ?? 'Default model'}</span>
          </div>
        </div>
        <div className="min-w-0 flex-1" />
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
          trigger={<IconButton icon={MoreVertical} label="Chat menu" className="size-8" />}
        />
      </header>

      <div
        ref={scrollRef}
        data-testid="chat-scroll-container"
        onScroll={(event) => {
          const target = event.currentTarget;
          pinnedRef.current = isChatScrolledToBottom(target);
        }}
        className="min-h-0 flex-1 overscroll-contain overflow-y-auto scroll-smooth"
      >
        <div className="mx-auto w-full px-7 pb-44 pt-10 md:w-[70%]">
          {detail.messages.map((message, index) => (
            <div key={message.id} data-chat-message-index={index}>
              <MessageBubble message={message} />
            </div>
          ))}
        </div>
      </div>
      <ChatDotNavigator messages={detail.messages} scrollContainerRef={scrollRef} />

      <div className="pointer-events-none absolute inset-x-0 bottom-0 z-10 bg-gradient-to-t from-bg via-bg/95 to-bg/0 px-7 pb-6 pt-12">
        <div className="pointer-events-auto mx-auto w-full md:w-[70%]">
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
            <>
              <Composer
                disabled={composerDisabled}
                placeholder={composerPlaceholder}
                initialModel={detail.pendingTurn?.options.model ?? detail.chat.model ?? undefined}
                persistModelPreference={false}
                onModelChange={handleModelChange}
                projectContext={detail.chat.projectId ? { projectId: detail.chat.projectId } : undefined}
                onSubmit={async ({ text, options }) => {
                  if (canQueueDeferredInput) {
                    await queueDeferredInput({ text, options });
                    return;
                  }
                  await startStream({ text, options, appendUser: true });
                }}
              />
              {detail.activeRun?.runId ? (
                <div className="mt-3 flex items-center justify-between gap-3 rounded-[16px] border border-border/70 bg-surface px-4 py-3 text-sm text-text-muted">
                  <p>
                    {canQueueDeferredInput
                      ? 'New messages are queued after the next tool call. Use Stop to interrupt now.'
                      : activeRunStatus === 'paused'
                        ? 'This run is paused. Resume to continue or Stop to cancel it.'
                        : activeRunBlocksNewInput
                          ? `Run status is ${detail.activeRun.status}. Stop it or refresh before sending new input.`
                          : 'Stopping the current run. New input stays disabled until cancellation completes.'}
                  </p>
                  <div className="flex shrink-0 items-center gap-2">
                    {canResumeRun ? (
                      <button
                        type="button"
                        onClick={() => { void resumeActiveRun(); }}
                        disabled={runControlBusy}
                        className="rounded-control bg-text px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                      >
                        {resumingRun ? 'Resuming...' : 'Resume'}
                      </button>
                    ) : null}
                    {canStopRun ? (
                      <button
                        type="button"
                        onClick={() => { void stopActiveRun(); }}
                        disabled={runControlBusy}
                        className="rounded-control border border-border bg-bg px-3 py-2 text-sm font-medium text-text transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50"
                      >
                        {stoppingRun ? 'Stopping...' : 'Stop'}
                      </button>
                    ) : null}
                  </div>
                </div>
              ) : null}
            </>
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
