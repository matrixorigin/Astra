"use client";

import { useRouter } from "next/navigation";
import {
  useCallback,
  useEffect,
  useRef,
  type Dispatch,
  type SetStateAction,
} from "react";
import {
  getChat,
  getEdgeStatus,
  getChatWorkSurface,
  getChatWorkSurfaceRun,
  queueChatRunInput,
  resumeChatRun,
  stopChatRun,
  streamChatMessage,
  streamExistingChatRun,
  updateChatModel,
} from "@/lib/api/chats";
import { mergeStreamRunUpdate } from "@/lib/api/active-run-merge";
import { createAssistantPatchController } from "@/lib/api/assistant-patch-controller";
import {
  WebApiError,
  isAuthRequiredError,
  isNotFoundError,
} from "@/lib/api/errors";
import type {
  ChatDetail,
  ChatMessage,
  ComposerOptions,
  WorkSurfaceRunResponse,
  WorkspaceSelection,
} from "@/lib/api/types";
import {
  applyWorkSurfaceEvent,
  beginWorkSurfaceLoad,
  createEmptyWorkSurface,
  failWorkSurfaceLoad,
  hydrateWorkSurface,
  resetWorkSurfaceForRun,
  type WorkSurfaceResponse as WorkSurfaceProjection,
} from "@/lib/work-surface";
import { useToast } from "@/components/ui/toast";

const STREAM_RECONCILE_INITIAL_DELAY_MS = 3_000;
const STREAM_RECONCILE_INTERVAL_MS = 5_000;
const STOP_RECONCILE_INITIAL_DELAY_MS = 500;
const STOP_RECONCILE_INTERVAL_MS = 1_000;
const STOP_RECONCILE_NOTICE_MS = 15_000;
const STOP_REQUEST_TIMEOUT_MS = 10_000;
const STOP_FAILURE_REFRESH_TIMEOUT_MS = 5_000;
const RUN_ATTACH_MAX_RETRIES = 4;
const ATTACHABLE_RUN_STATUSES = new Set([
  "running",
  "blocked",
  "input-queued",
  "waiting",
  "cancelling",
]);

// Re-exported helpers
export { ATTACHABLE_RUN_STATUSES };

function isAbortError(error: unknown) {
  return error instanceof DOMException && error.name === "AbortError";
}

function isWorkspaceSelectionError(error: unknown): error is WebApiError {
  return (
    error instanceof WebApiError &&
    error.status === 409 &&
    typeof error.code === "string" &&
    error.code.startsWith("workspace_")
  );
}

function hasCompletedAssistantAfterUser(
  detail: ChatDetail,
  userMessageId: string,
) {
  const userIndex = detail.messages.findIndex((m) => m.id === userMessageId);
  if (userIndex === -1) return false;
  for (let i = userIndex + 1; i < detail.messages.length; i++) {
    const message = detail.messages[i];
    if (
      message.role === "assistant" &&
      message.status !== "streaming" &&
      message.content.trim()
    ) {
      return true;
    }
  }
  return false;
}

export function canAttachRunStream(status: string | undefined | null) {
  return status ? ATTACHABLE_RUN_STATUSES.has(status) : false;
}

function findStreamingAssistantMessageId(messages: ChatMessage[]) {
  return [...messages]
    .reverse()
    .find(
      (message) =>
        message.role === "assistant" &&
        (message.status === "streaming" ||
          message.reasoningStatus === "streaming"),
    )?.id;
}

function completeLatestStreamingAssistantAsStopped(messages: ChatMessage[]) {
  const index = [...messages]
    .reverse()
    .findIndex(
      (message) =>
        message.role === "assistant" &&
        (message.status === "streaming" ||
          message.reasoningStatus === "streaming"),
    );
  if (index < 0) {
    return messages;
  }
  const actualIndex = messages.length - 1 - index;
  return messages.map((message, i) =>
    i === actualIndex
      ? {
          ...message,
          status: "complete" as const,
          content: message.content.trim()
            ? `${message.content}${message.content.endsWith("\n") ? "" : "\n"}\nStopped.`
            : "Stopped.",
          completedAt: new Date().toISOString(),
          reasoningStatus: "complete" as const,
        }
      : message,
  );
}

function withClientTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  message: string,
): Promise<T> {
  let timeoutId: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timeoutId = setTimeout(() => reject(new Error(message)), timeoutMs);
  });
  return Promise.race([promise, timeout]).finally(() => {
    if (timeoutId !== undefined) {
      clearTimeout(timeoutId);
    }
  });
}

export interface UseStreamLifecycleParams {
  detail: ChatDetail;
  setDetail: Dispatch<SetStateAction<ChatDetail>>;
  isArchived: boolean;
  chatListHref: string;
  workspaceSelection: WorkspaceSelection | null;
  workspaceSelectionExplicit: boolean;
  canStopRun: boolean;
  canResumeRun: boolean;
  canQueueDeferredInput: boolean;
  setStartingRun: Dispatch<SetStateAction<boolean>>;
  setQueueingDeferredInput: Dispatch<SetStateAction<boolean>>;
  setResumingRun: Dispatch<SetStateAction<boolean>>;
  setStoppingRun: Dispatch<SetStateAction<boolean>>;
  setWorkSurface: Dispatch<
    SetStateAction<ReturnType<typeof createEmptyWorkSurface>>
  >;
  runAttachRetrySignal: number;
  setRunAttachRetrySignal: Dispatch<SetStateAction<number>>;
  refreshEdgeWorkspaces: () => Promise<void>;
}

export interface UseStreamLifecycleReturn {
  nextStreamAbortSignal: () => AbortSignal;
  applyWorkSurfaceStreamEvent: (event: Record<string, unknown>) => void;
  resetWorkSurfaceRun: (
    runId: string | null,
    sessionId?: string | null,
  ) => void;
  ensureStreamingAssistantMessage: (
    preferredMessageId?: string | null,
  ) => string;
  scheduleAutoAttachRetry: (runId: string) => boolean;
  attachExistingRunStream: (
    runId: string,
    assistantMessageId: string,
    failureMessage: string,
    options?: { scheduleRetry?: () => boolean },
  ) => void;
  startStream: (params: {
    text: string;
    options: ComposerOptions;
    pendingMessageId?: string;
    appendUser: boolean;
  }) => Promise<void>;
  queueDeferredInput: (params: {
    text: string;
    options: ComposerOptions;
  }) => Promise<void>;
  stopActiveRun: () => void;
  resumeActiveRun: () => Promise<void>;
  hydrateWorkSurfaceForChat: (options?: { silent?: boolean }) => Promise<void>;
  loadAgentRunProjection: (runId: string) => Promise<WorkSurfaceRunResponse>;
  refresh: () => Promise<void>;
  handleModelChange: (model: string) => void;
  // Refs exposed for effect deps
  streamAbortRef: React.RefObject<AbortController | null>;
  attachedRunRef: React.RefObject<string | null>;
  autoAttachAttemptedRunRef: React.RefObject<string | null>;
  autoAttachRetryTimerRef: React.RefObject<number | undefined>;
  autoAttachRetryCountsRef: React.RefObject<Map<string, number>>;
  reconcileTimerRef: React.RefObject<number | undefined>;
  reconcileIntervalRef: React.RefObject<number | undefined>;
  stopReconcileRef: React.RefObject<() => void>;
  runControlMutationRef: React.RefObject<boolean>;
}

export function useStreamLifecycle(
  params: UseStreamLifecycleParams,
): UseStreamLifecycleReturn {
  const router = useRouter();
  const { addToast } = useToast();

  const {
    detail,
    setDetail,
    isArchived,
    chatListHref,
    workspaceSelection,
    workspaceSelectionExplicit,
    canStopRun,
    canResumeRun,
    canQueueDeferredInput,
    setStartingRun,
    setResumingRun,
    setStoppingRun,
    setQueueingDeferredInput,
    setWorkSurface,
    runAttachRetrySignal: _runAttachRetrySignal,
    setRunAttachRetrySignal: _setRunAttachRetrySignal,
    refreshEdgeWorkspaces,
  } = params;

  // Refs
  const streamAbortRef = useRef<AbortController | null>(null);
  const attachedRunRef = useRef<string | null>(null);
  const attachedRunLeaseRef = useRef(0);
  const autoAttachAttemptedRunRef = useRef<string | null>(null);
  const autoAttachRetryTimerRef = useRef<number | undefined>(undefined);
  const autoAttachRetryCountsRef = useRef<Map<string, number>>(new Map());
  const reconcileTimerRef = useRef<number | undefined>(undefined);
  const reconcileIntervalRef = useRef<number | undefined>(undefined);
  const stopReconcileRef = useRef<() => void>(() => {
    if (reconcileTimerRef.current) {
      window.clearTimeout(reconcileTimerRef.current);
      reconcileTimerRef.current = undefined;
    }
    if (reconcileIntervalRef.current) {
      window.clearInterval(reconcileIntervalRef.current);
      reconcileIntervalRef.current = undefined;
    }
  });
  const runControlMutationRef = useRef(false);

  // -- Stream signal helpers --
  const nextStreamAbortSignal = useCallback(() => {
    streamAbortRef.current?.abort();
    const controller = new AbortController();
    streamAbortRef.current = controller;
    return controller.signal;
  }, []);

  const claimAttachedRun = useCallback((runId: string) => {
    attachedRunLeaseRef.current += 1;
    const lease = attachedRunLeaseRef.current;
    attachedRunRef.current = runId;
    return lease;
  }, []);

  const clearAttachedRun = useCallback((runId: string, lease?: number) => {
    if (lease === undefined) {
      attachedRunLeaseRef.current += 1;
      if (attachedRunRef.current === runId) {
        attachedRunRef.current = null;
      }
      return;
    }
    if (
      attachedRunLeaseRef.current === lease &&
      attachedRunRef.current === runId
    ) {
      attachedRunRef.current = null;
    }
  }, []);

  const applyWorkSurfaceStreamEvent = useCallback(
    (event: Record<string, unknown>) => {
      setWorkSurface((current) => applyWorkSurfaceEvent(current, event));
    },
    [setWorkSurface],
  );

  const resetWorkSurfaceRun = useCallback(
    (runId: string | null, sessionId?: string | null) => {
      setWorkSurface((current) =>
        resetWorkSurfaceForRun(current, {
          sessionId:
            sessionId === undefined
              ? (detail.session?.backendSessionId ?? current.sessionId)
              : sessionId,
          runId,
        }),
      );
    },
    [detail.session?.backendSessionId, setWorkSurface],
  );

  const ensureStreamingAssistantMessage = useCallback(
    (preferredMessageId?: string | null) => {
      if (
        preferredMessageId &&
        detail.messages.some(
          (message) =>
            message.id === preferredMessageId && message.role === "assistant",
        )
      ) {
        return preferredMessageId;
      }
      const existingAssistantMessageId = findStreamingAssistantMessageId(
        detail.messages,
      );
      if (existingAssistantMessageId) {
        return existingAssistantMessageId;
      }
      const assistantMessageId = preferredMessageId ?? crypto.randomUUID();
      setDetail((current) =>
        current.messages.some(
          (message) =>
            message.id === assistantMessageId && message.role === "assistant",
        ) || findStreamingAssistantMessageId(current.messages)
          ? current
          : {
              ...current,
              messages: [
                ...current.messages,
                {
                  id: assistantMessageId,
                  role: "assistant" as const,
                  content: "",
                  createdAt: new Date().toISOString(),
                  status: "streaming" as const,
                  reasoning: "",
                  reasoningStatus: "streaming" as const,
                },
              ],
            },
      );
      return assistantMessageId;
    },
    [detail.messages, setDetail],
  );

  const scheduleAutoAttachRetry = useCallback(
    (runId: string) => {
      const attempts = autoAttachRetryCountsRef.current.get(runId) ?? 0;
      if (attempts >= RUN_ATTACH_MAX_RETRIES) {
        return false;
      }
      const nextAttempts = attempts + 1;
      autoAttachRetryCountsRef.current.set(runId, nextAttempts);
      if (autoAttachRetryTimerRef.current) {
        window.clearTimeout(autoAttachRetryTimerRef.current);
      }
      const delayMs = Math.min(1_000 * 2 ** attempts, 8_000);
      autoAttachRetryTimerRef.current = window.setTimeout(() => {
        autoAttachRetryTimerRef.current = undefined;
        if (autoAttachAttemptedRunRef.current === runId) {
          autoAttachAttemptedRunRef.current = null;
        }
        _setRunAttachRetrySignal((signal) => signal + 1);
      }, delayMs);
      return true;
    },
    [_setRunAttachRetrySignal],
  );

  // -- Hydrate work surface --
  const hydrateWorkSurfaceForChat = useCallback(
    async (options?: { silent?: boolean }) => {
      const sessionId = detail.session?.backendSessionId ?? null;
      if (!options?.silent) {
        setWorkSurface((current) =>
          beginWorkSurfaceLoad(current, sessionId, current.runId),
        );
      }
      try {
        const response = await getChatWorkSurface(detail.chat.id);
        setWorkSurface((current) =>
          hydrateWorkSurface(
            current,
            response as unknown as WorkSurfaceProjection,
          ),
        );
      } catch (error) {
        setWorkSurface((current) =>
          failWorkSurfaceLoad(
            current,
            error instanceof Error
              ? error.message
              : "Failed to load activity.",
          ),
        );
      }
    },
    [detail.chat.id, detail.session?.backendSessionId, setWorkSurface],
  );

  const loadAgentRunProjection = useCallback(
    (runId: string) => getChatWorkSurfaceRun(detail.chat.id, runId),
    [detail.chat.id],
  );

  // -- Attach existing run stream --
  const attachExistingRunStream = useCallback(
    (
      runId: string,
      assistantMessageId: string,
      failureMessage: string,
      options?: { scheduleRetry?: () => boolean },
    ) => {
      if (attachedRunRef.current === runId) {
        return;
      }

      const attachLease = claimAttachedRun(runId);
      resetWorkSurfaceRun(runId);
      const assistantPatcher = createAssistantPatchController({
        setDetail,
        getAssistantId: () => assistantMessageId,
      });
      const clearActiveRun = () => {
        setDetail((current) =>
          current.activeRun?.runId === runId
            ? {
                ...current,
                activeRun: undefined,
              }
            : current,
        );
      };

      void streamExistingChatRun(
        detail.chat.id,
        runId,
        {
          signal: nextStreamAbortSignal(),
          onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
          onRunUpdated: (run) => {
            setDetail((current) => ({
              ...current,
              activeRun: mergeStreamRunUpdate(
                { ...run, assistantMessageId },
                current.activeRun,
              ),
            }));
          },
          onRunFinished: () => {
            setStoppingRun(false);
            clearActiveRun();
          },
          onReasoning: (reasoning) => {
            assistantPatcher.patchBatched({
              reasoning,
              reasoningStatus: "streaming",
              status: "streaming",
            });
          },
          onReasoningDone: (reasoning) => {
            assistantPatcher.patchBatched({
              reasoning,
              reasoningStatus: "complete",
              status: "streaming",
            });
          },
          onText: (content) => {
            assistantPatcher.patchBatched({ content, status: "streaming" });
          },
          onArtifacts: (artifacts) => {
            assistantPatcher.patchBatched({ artifacts });
          },
          onDone: (content) => {
            assistantPatcher.flushNow();
            clearActiveRun();
            assistantPatcher.patchNow({
              content:
                content ||
                "Astra completed the run without returning visible text.",
              completedAt: new Date().toISOString(),
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onCancelled: (content) => {
            assistantPatcher.flushNow();
            clearActiveRun();
            assistantPatcher.patchNow({
              content: content || "Stopped.",
              completedAt: new Date().toISOString(),
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onPaused: (content) => {
            assistantPatcher.flushNow();
            assistantPatcher.patchNow({
              ...(content ? { content } : {}),
              completedAt: new Date().toISOString(),
              reasoningStatus: "complete",
              status: "complete",
            });
          },
        },
        {
          assistantMessageId,
          nextEventIndex:
            detail.activeRun?.runId === runId
              ? (detail.activeRun.nextEventIndex ?? null)
              : null,
        },
      )
        .catch((streamError: unknown) => {
          if (isAbortError(streamError)) {
            return;
          }
          if (isAuthRequiredError(streamError)) {
            router.push(
              `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
            );
            return;
          }
          if (isNotFoundError(streamError)) {
            router.replace(chatListHref);
            return;
          }
          if (options?.scheduleRetry?.()) {
            assistantPatcher.patchNow({
              status: "streaming",
              reasoningStatus: "streaming",
            });
            return;
          }
          assistantPatcher.patchNow({
            content:
              streamError instanceof Error
                ? `${failureMessage} (${streamError.message})`
                : failureMessage,
            completedAt: new Date().toISOString(),
            reasoningStatus: "complete",
            status: "failed",
          });
        })
        .finally(() => {
          clearAttachedRun(runId, attachLease);
          assistantPatcher.cancel();
        });
    },
    [
      applyWorkSurfaceStreamEvent,
      chatListHref,
      claimAttachedRun,
      clearAttachedRun,
      detail.activeRun?.nextEventIndex,
      detail.activeRun?.runId,
      detail.chat.id,
      nextStreamAbortSignal,
      resetWorkSurfaceRun,
      setDetail,
      setStoppingRun,
      router,
    ],
  );

  // -- Start new stream --
  const startStream = useCallback(
    async ({
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
      let currentAssistantId = assistantId;
      let currentUserId = pendingMessageId ?? crypto.randomUUID();
      const userMessage: ChatMessage = {
        id: currentUserId,
        role: "user",
        content: text,
        activeSkills: options.activeSkills,
        createdAt: timestamp,
        status: "complete",
      };
      const assistantMessage: ChatMessage = {
        id: assistantId,
        role: "assistant",
        content: "",
        createdAt: timestamp,
        status: "streaming",
        reasoning: "",
        reasoningStatus: "streaming",
      };

      const assistantPatcher = createAssistantPatchController({
        setDetail,
        getAssistantId: () => currentAssistantId,
      });
      let recoveredFromHydration = false;
      let streamRunId: string | null = null;
      let streamRunLease: number | null = null;
      const canReconcilePersistedTranscript = Boolean(pendingMessageId);
      const stopReconcile = () => {
        stopReconcileRef.current();
        reconcileTimerRef.current = undefined;
        reconcileIntervalRef.current = undefined;
      };
      const reconcilePersistedTranscript = async () => {
        if (!canReconcilePersistedTranscript || recoveredFromHydration) {
          return;
        }
        const refreshed = await getChat(detail.chat.id).catch(() => null);
        if (
          !refreshed ||
          !hasCompletedAssistantAfterUser(refreshed, pendingMessageId!)
        ) {
          return;
        }
        recoveredFromHydration = true;
        stopReconcile();
        setStartingRun(false);
        setStoppingRun(false);
        setDetail(refreshed);
        streamAbortRef.current?.abort();
      };
      if (canReconcilePersistedTranscript) {
        reconcileTimerRef.current = window.setTimeout(() => {
          void reconcilePersistedTranscript();
          reconcileIntervalRef.current = window.setInterval(() => {
            void reconcilePersistedTranscript();
          }, STREAM_RECONCILE_INTERVAL_MS);
        }, STREAM_RECONCILE_INITIAL_DELAY_MS);
      }

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
      const streamPayload = {
        content: text,
        options,
        pendingMessageId,
        ...(workspaceSelectionExplicit && workspaceSelection
          ? { workspace: workspaceSelection }
          : {}),
      };
      try {
        await streamChatMessage(detail.chat.id, streamPayload, {
          signal: nextStreamAbortSignal(),
          onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
          onLocalMessages: ({
            userMessage: localUserMessage,
            assistantMessage: localAssistantMessage,
          }) => {
            const previousAssistantId = currentAssistantId;
            const previousUserId = currentUserId;
            currentAssistantId = localAssistantMessage.id;
            currentUserId = localUserMessage.id;
            setDetail((current) => {
              let sawAssistant = false;
              let sawUser = false;
              const messages = current.messages.flatMap((message) => {
                if (
                  message.id === previousUserId ||
                  message.id === localUserMessage.id
                ) {
                  if (sawUser) {
                    return [];
                  }
                  sawUser = true;
                  return [localUserMessage];
                }
                if (
                  message.id === previousAssistantId ||
                  message.id === localAssistantMessage.id
                ) {
                  if (sawAssistant) {
                    return [];
                  }
                  sawAssistant = true;
                  return [localAssistantMessage];
                }
                return [message];
              });
              if (!sawUser && appendUser) {
                messages.push(localUserMessage);
              }
              if (!sawAssistant) {
                messages.push(localAssistantMessage);
              }
              return {
                ...current,
                pendingTurn: pendingMessageId ? undefined : current.pendingTurn,
                messages,
              };
            });
          },
          onRunStarted: (runId) => {
            streamRunId = runId;
            streamRunLease = claimAttachedRun(runId);
            resetWorkSurfaceRun(runId);
            setStartingRun(false);
            setDetail((current) => ({
              ...current,
              activeRun: {
                runId,
                status: "running",
                waitingFor: null,
                assistantMessageId: currentAssistantId,
                nextEventIndex: null,
              },
            }));
          },
          onSessionBound: ({ sessionId }) => {
            setWorkSurface((current) => ({
              ...resetWorkSurfaceForRun(current, {
                sessionId,
                runId: current.runId,
              }),
              error: null,
            }));
            setDetail((current) => ({
              ...current,
              session: {
                chatId: current.session?.chatId ?? current.chat.id,
                backendSessionId: sessionId,
                persisted: true,
                messageCount: current.messages.length,
              },
            }));
          },
          onRunUpdated: (run) => {
            setDetail((current) => ({
              ...current,
              activeRun: mergeStreamRunUpdate(
                { ...run, assistantMessageId: currentAssistantId },
                current.activeRun,
              ),
            }));
          },
          onRunFinished: () => {
            setStoppingRun(false);
            setStartingRun(false);
            setDetail((current) =>
              streamRunId && current.activeRun?.runId === streamRunId
                ? {
                    ...current,
                    activeRun: undefined,
                  }
                : current,
            );
          },
          onReasoning: (reasoning) => {
            assistantPatcher.patchBatched({
              reasoning,
              reasoningStatus: "streaming",
              status: "streaming",
            });
          },
          onReasoningDone: (reasoning) => {
            assistantPatcher.patchBatched({
              reasoning,
              reasoningStatus: "complete",
              status: "streaming",
            });
          },
          onText: (content) => {
            assistantPatcher.patchBatched({ content, status: "streaming" });
          },
          onArtifacts: (artifacts) => {
            assistantPatcher.patchBatched({ artifacts });
          },
          onDone: (content) => {
            assistantPatcher.flushNow();
            setStartingRun(false);
            setDetail((current) =>
              streamRunId && current.activeRun?.runId === streamRunId
                ? {
                    ...current,
                    activeRun: undefined,
                  }
                : current,
            );
            assistantPatcher.patchNow({
              content:
                content ||
                "Astra completed the run without returning visible text.",
              completedAt: new Date().toISOString(),
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onCancelled: (content) => {
            assistantPatcher.flushNow();
            setStoppingRun(false);
            setStartingRun(false);
            setDetail((current) =>
              streamRunId && current.activeRun?.runId === streamRunId
                ? {
                    ...current,
                    activeRun: undefined,
                  }
                : current,
            );
            assistantPatcher.patchNow({
              content: content || "Stopped.",
              completedAt: new Date().toISOString(),
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onPaused: (content) => {
            assistantPatcher.flushNow();
            assistantPatcher.patchNow({
              ...(content ? { content } : {}),
              completedAt: new Date().toISOString(),
              reasoningStatus: "complete",
              status: "complete",
            });
          },
        });
      } catch (error) {
        if (isAbortError(error)) {
          return;
        }
        if (isAuthRequiredError(error)) {
          router.push(
            `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
          );
          return;
        }
        if (isNotFoundError(error)) {
          const targetAssistantId = currentAssistantId;
          const targetUserId = currentUserId;
          setDetail((current) => ({
            ...current,
            messages: current.messages.filter(
              (message) =>
                message.id !== targetAssistantId &&
                (!appendUser || message.id !== targetUserId),
            ),
          }));
          router.replace(chatListHref);
          return;
        }
        if (isWorkspaceSelectionError(error)) {
          setStartingRun(false);
          void refreshEdgeWorkspaces();
          assistantPatcher.patchNow({
            content: error.detail,
            completedAt: new Date().toISOString(),
            reasoningStatus: "complete",
            status: "failed",
          });
          return;
        }
        const message =
          error instanceof Error ? error.message : "Astra stream failed.";
        assistantPatcher.patchNow({
          content: `I could not reach the Astra runtime from the web UI. (${message})`,
          completedAt: new Date().toISOString(),
          status: "failed",
        });
      } finally {
        if (streamRunId && streamRunLease !== null) {
          clearAttachedRun(streamRunId, streamRunLease);
        }
        assistantPatcher.cancel();
        stopReconcile();
        setStartingRun(false);
      }
    },
    [
      applyWorkSurfaceStreamEvent,
      chatListHref,
      claimAttachedRun,
      clearAttachedRun,
      detail.chat.archivedAt,
      detail.chat.id,
      nextStreamAbortSignal,
      refreshEdgeWorkspaces,
      resetWorkSurfaceRun,
      setDetail,
      setStartingRun,
      setStoppingRun,
      setWorkSurface,
      workspaceSelection,
      workspaceSelectionExplicit,
      router,
    ],
  );

  // -- Queue deferred input --
  const queueDeferredInput = useCallback(
    async ({ text, options }: { text: string; options: ComposerOptions }) => {
      if (detail.chat.archivedAt) {
        return;
      }
      if (runControlMutationRef.current) {
        return;
      }
      runControlMutationRef.current = true;
      setQueueingDeferredInput(true);

      try {
        const pendingMessageId = crypto.randomUUID();
        const result = await queueChatRunInput(detail.chat.id, {
          content: text,
          options,
          pendingMessageId,
        });
        const assistantMessageId = result.assistantMessage.id;
        const runId = result.activeRun.runId;
        const attachLease = claimAttachedRun(runId);
        resetWorkSurfaceRun(runId);
        setDetail((current) => ({
          ...current,
          activeRun: result.activeRun,
          messages: [
            ...current.messages.filter(
              (message) =>
                message.id !== result.userMessage.id &&
                message.id !== result.assistantMessage.id,
            ),
            result.userMessage,
            result.assistantMessage,
          ].sort(
            (a, b) =>
              new Date(a.createdAt).getTime() - new Date(b.createdAt).getTime(),
          ),
        }));
        const assistantPatcher = createAssistantPatchController({
          setDetail,
          getAssistantId: () => assistantMessageId,
        });
        void streamExistingChatRun(
          detail.chat.id,
          runId,
          {
            signal: nextStreamAbortSignal(),
            onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
            onRunUpdated: (run) => {
              setDetail((current) => ({
                ...current,
                activeRun: mergeStreamRunUpdate(
                  { ...run, assistantMessageId },
                  current.activeRun,
                ),
              }));
            },
            onRunFinished: () => {
              setStoppingRun(false);
              setDetail((current) =>
                current.activeRun?.runId === runId
                  ? {
                      ...current,
                      activeRun: undefined,
                    }
                  : current,
              );
            },
            onReasoning: (reasoning) => {
              assistantPatcher.patchBatched({
                reasoning,
                reasoningStatus: "streaming",
                status: "streaming",
              });
            },
            onReasoningDone: (reasoning) => {
              assistantPatcher.patchBatched({
                reasoning,
                reasoningStatus: "complete",
                status: "streaming",
              });
            },
            onText: (content) => {
              assistantPatcher.patchBatched({ content, status: "streaming" });
            },
            onArtifacts: (artifacts) => {
              assistantPatcher.patchBatched({ artifacts });
            },
            onDone: (content) => {
              assistantPatcher.flushNow();
              setDetail((current) =>
                current.activeRun?.runId === runId
                  ? {
                      ...current,
                      activeRun: undefined,
                    }
                  : current,
              );
              assistantPatcher.patchNow({
                content:
                  content ||
                  "Astra completed the run without returning visible text.",
                completedAt: new Date().toISOString(),
                reasoningStatus: "complete",
                status: "complete",
              });
            },
            onCancelled: (content) => {
              assistantPatcher.flushNow();
              setDetail((current) =>
                current.activeRun?.runId === runId
                  ? {
                      ...current,
                      activeRun: undefined,
                    }
                  : current,
              );
              assistantPatcher.patchNow({
                content: content || "Stopped.",
                completedAt: new Date().toISOString(),
                reasoningStatus: "complete",
                status: "complete",
              });
            },
            onPaused: (content) => {
              assistantPatcher.flushNow();
              assistantPatcher.patchNow({
                ...(content ? { content } : {}),
                completedAt: new Date().toISOString(),
                reasoningStatus: "complete",
                status: "complete",
              });
            },
          },
          {
            assistantMessageId,
            nextEventIndex: result.activeRun.nextEventIndex ?? null,
          },
        )
          .catch((streamError: unknown) => {
            if (isAbortError(streamError)) {
              return;
            }
            if (isAuthRequiredError(streamError)) {
              router.push(
                `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
              );
              return;
            }
            if (isNotFoundError(streamError)) {
              router.replace(chatListHref);
              return;
            }
            assistantPatcher.patchNow({
              content:
                streamError instanceof Error
                  ? `The input was queued, but the web UI could not reconnect to the run stream. (${streamError.message})`
                  : "The input was queued, but the web UI could not reconnect to the run stream.",
              completedAt: new Date().toISOString(),
              status: "failed",
            });
          })
          .finally(() => {
            clearAttachedRun(runId, attachLease);
            assistantPatcher.cancel();
          });
      } catch (error) {
        if (isAuthRequiredError(error)) {
          router.push(
            `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
          );
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
              void startStream({ text, options, appendUser: true });
              return;
            }
          }
        }
        addToast(
          error instanceof Error
            ? error.message
            : "Failed to queue input for the active run.",
          "error",
        );
      } finally {
        runControlMutationRef.current = false;
        setQueueingDeferredInput(false);
      }
    },
    [
      addToast,
      applyWorkSurfaceStreamEvent,
      chatListHref,
      claimAttachedRun,
      clearAttachedRun,
      detail.chat.archivedAt,
      detail.chat.id,
      nextStreamAbortSignal,
      resetWorkSurfaceRun,
      setDetail,
      setQueueingDeferredInput,
      setStoppingRun,
      router,
      startStream,
    ],
  );

  // -- Stop active run --
  const stopActiveRun = useCallback(() => {
    if (!detail.activeRun?.runId || !canStopRun) {
      return;
    }
    const runId = detail.activeRun.runId;
    const previousActiveRun = detail.activeRun;
    const previousMessages = detail.messages;
    if (runControlMutationRef.current) {
      return;
    }
    runControlMutationRef.current = true;
    setStoppingRun(true);
    streamAbortRef.current?.abort();
    clearAttachedRun(runId);
    setDetail((current) => ({
      ...current,
      activeRun:
        current.activeRun?.runId === runId
          ? {
              ...current.activeRun,
              status: "cancelling",
              waitingFor: "cancel_requested",
            }
          : current.activeRun,
      messages: completeLatestStreamingAssistantAsStopped(current.messages),
    }));
    void hydrateWorkSurfaceForChat({ silent: true });
    void withClientTimeout(
      stopChatRun(detail.chat.id),
      STOP_REQUEST_TIMEOUT_MS,
      "Stop request timed out.",
    )
      .then((result) => {
        setDetail((current) => ({
          ...current,
          activeRun: result.activeRun,
        }));
        void hydrateWorkSurfaceForChat({ silent: true });
        if (result.cancelPending) {
          const startedAt = Date.now();
          let noticeShown = false;
          const reconcileStop = async () => {
            const refreshed = await getChat(detail.chat.id).catch(() => null);
            if (!refreshed) {
              return;
            }
            setDetail(refreshed);
            void hydrateWorkSurfaceForChat({ silent: true });
            const activeRun = refreshed.activeRun;
            const stillCancelling =
              activeRun?.runId === runId && activeRun.status === "cancelling";
            if (!stillCancelling) {
              stopReconcileRef.current();
              return;
            }
            if (
              !noticeShown &&
              Date.now() - startedAt >= STOP_RECONCILE_NOTICE_MS
            ) {
              noticeShown = true;
              addToast(
                "Stop request is still being processed by the runtime.",
                "info",
              );
            }
          };
          stopReconcileRef.current();
          reconcileTimerRef.current = window.setTimeout(() => {
            void reconcileStop();
            reconcileIntervalRef.current = window.setInterval(() => {
              void reconcileStop();
            }, STOP_RECONCILE_INTERVAL_MS);
          }, STOP_RECONCILE_INITIAL_DELAY_MS);
        }
      })
      .catch((error: unknown) => {
        if (isAuthRequiredError(error)) {
          router.push(
            `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
          );
          return;
        }
        if (isNotFoundError(error)) {
          router.replace(chatListHref);
          return;
        }
        void withClientTimeout(
          getChat(detail.chat.id),
          STOP_FAILURE_REFRESH_TIMEOUT_MS,
          "Timed out refreshing the active run after stop failed.",
        )
          .then((refreshed) => {
            setDetail(refreshed);
            void hydrateWorkSurfaceForChat({ silent: true });
          })
          .catch(() => {
            setDetail((current) => {
              if (
                current.activeRun?.runId !== runId ||
                current.activeRun.status !== "cancelling"
              ) {
                return current;
              }
              return {
                ...current,
                activeRun: previousActiveRun,
                messages: previousMessages,
              };
            });
            void hydrateWorkSurfaceForChat({ silent: true });
          });
        addToast(
          error instanceof Error
            ? error.message
            : "Failed to stop the active run.",
          "error",
        );
      })
      .finally(() => {
        runControlMutationRef.current = false;
        setStoppingRun(false);
      });
  }, [
    addToast,
    canStopRun,
    chatListHref,
    clearAttachedRun,
    detail.activeRun,
    detail.chat.id,
    detail.messages,
    hydrateWorkSurfaceForChat,
    reconcileIntervalRef,
    reconcileTimerRef,
    setDetail,
    setStoppingRun,
    stopReconcileRef,
    router,
  ]);

  // -- Resume active run --
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
      .find(
        (message) =>
          message.role === "assistant" &&
          (message.status === "streaming" ||
            message.reasoningStatus === "streaming"),
      )?.id;
    const assistantMessageId =
      existingAssistantMessageId ?? crypto.randomUUID();
    setResumingRun(true);
    let optimisticMessageId: string | null = null;
    try {
      const result = await resumeChatRun(detail.chat.id);
      if (!result.activeRun?.runId) {
        throw new Error("Resume response did not include an active run.");
      }
      const resumedActiveRun = result.activeRun;
      const runId = resumedActiveRun.runId;
      const attachLease = claimAttachedRun(runId);
      resetWorkSurfaceRun(runId);
      const appendedOptimistic =
        !existingAssistantMessageId &&
        !detail.messages.some((message) => message.id === assistantMessageId);
      if (appendedOptimistic) {
        optimisticMessageId = assistantMessageId;
      }
      setDetail((current) => ({
        ...current,
        activeRun: {
          ...resumedActiveRun,
          assistantMessageId,
        },
        messages:
          existingAssistantMessageId ||
          current.messages.some((message) => message.id === assistantMessageId)
            ? current.messages
            : [
                ...current.messages,
                {
                  id: assistantMessageId,
                  role: "assistant",
                  content: "",
                  createdAt: new Date().toISOString(),
                  status: "streaming",
                  reasoning: "",
                  reasoningStatus: "streaming",
                },
              ],
      }));
      const assistantPatcher = createAssistantPatchController({
        setDetail,
        getAssistantId: () => assistantMessageId,
      });
      try {
        await streamExistingChatRun(
          detail.chat.id,
          runId,
          {
            signal: nextStreamAbortSignal(),
            onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
            onRunUpdated: (run) => {
              setDetail((current) => ({
                ...current,
                activeRun: mergeStreamRunUpdate(
                  { ...run, assistantMessageId },
                  current.activeRun,
                ),
              }));
            },
            onRunFinished: () => {
              setStoppingRun(false);
              setDetail((current) =>
                current.activeRun?.runId === runId
                  ? {
                      ...current,
                      activeRun: undefined,
                    }
                  : current,
              );
            },
            onReasoning: (reasoning) => {
              assistantPatcher.patchBatched({
                reasoning,
                reasoningStatus: "streaming",
                status: "streaming",
              });
            },
            onReasoningDone: (reasoning) => {
              assistantPatcher.patchBatched({
                reasoning,
                reasoningStatus: "complete",
                status: "streaming",
              });
            },
            onText: (content) => {
              assistantPatcher.patchBatched({ content, status: "streaming" });
            },
            onArtifacts: (artifacts) => {
              assistantPatcher.patchBatched({ artifacts });
            },
            onDone: (content) => {
              assistantPatcher.flushNow();
              setDetail((current) =>
                current.activeRun?.runId === runId
                  ? {
                      ...current,
                      activeRun: undefined,
                    }
                  : current,
              );
              assistantPatcher.patchNow({
                content:
                  content ||
                  "Astra completed the run without returning visible text.",
                completedAt: new Date().toISOString(),
                reasoningStatus: "complete",
                status: "complete",
              });
            },
            onCancelled: (content) => {
              assistantPatcher.flushNow();
              setDetail((current) =>
                current.activeRun?.runId === runId
                  ? {
                      ...current,
                      activeRun: undefined,
                    }
                  : current,
              );
              assistantPatcher.patchNow({
                content: content || "Stopped.",
                completedAt: new Date().toISOString(),
                reasoningStatus: "complete",
                status: "complete",
              });
            },
            onPaused: (content) => {
              assistantPatcher.flushNow();
              assistantPatcher.patchNow({
                ...(content ? { content } : {}),
                completedAt: new Date().toISOString(),
                reasoningStatus: "complete",
                status: "complete",
              });
            },
          },
          {
            assistantMessageId,
            nextEventIndex:
              resumedActiveRun.nextEventIndex ??
              detail.activeRun?.nextEventIndex ??
              null,
          },
        );
      } catch (streamError) {
        if (isAbortError(streamError)) {
          return;
        }
        if (isAuthRequiredError(streamError)) {
          router.push(
            `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
          );
          return;
        }
        if (isNotFoundError(streamError)) {
          router.replace(chatListHref);
          return;
        }
        // Roll back optimistic message on stream error
        if (optimisticMessageId) {
          setDetail((current) => ({
            ...current,
            messages: current.messages.filter(
              (message) => message.id !== optimisticMessageId,
            ),
          }));
        }
        try {
          const refreshed = await getChat(detail.chat.id);
          setDetail(refreshed);
        } catch (refreshError) {
          console.warn(
            "[stream-lifecycle] failed to refresh chat after stream error:",
            refreshError,
          );
          // Keep the local running state if refresh also fails; the alert still
          // tells the user that stream reconnection did not attach.
        }
        addToast(
          streamError instanceof Error
            ? `The run resumed, but the web UI could not reconnect to its stream. (${streamError.message})`
            : "The run resumed, but the web UI could not reconnect to its stream.",
          "warning",
        );
      } finally {
        clearAttachedRun(runId, attachLease);
        assistantPatcher.cancel();
      }
    } catch (error) {
      if (isAbortError(error)) {
        return;
      }
      if (isAuthRequiredError(error)) {
        router.push(
          `/login?next=${encodeURIComponent(`/chats/${detail.chat.id}`)}`,
        );
        return;
      }
      if (isNotFoundError(error)) {
        router.replace(chatListHref);
        return;
      }
      // Roll back optimistic message on resume error
      if (optimisticMessageId) {
        setDetail((current) => ({
          ...current,
          messages: current.messages.filter(
            (message) => message.id !== optimisticMessageId,
          ),
        }));
      }
      addToast(
        error instanceof Error
          ? error.message
          : "Failed to resume the paused run.",
        "error",
      );
    } finally {
      runControlMutationRef.current = false;
      setResumingRun(false);
    }
  }, [
    addToast,
    applyWorkSurfaceStreamEvent,
    canResumeRun,
    chatListHref,
    claimAttachedRun,
    clearAttachedRun,
    detail.activeRun?.nextEventIndex,
    detail.activeRun?.runId,
    detail.chat.id,
    detail.messages,
    nextStreamAbortSignal,
    resetWorkSurfaceRun,
    setDetail,
    setResumingRun,
    setStoppingRun,
    router,
  ]);

  // -- Refresh --
  const refresh = useCallback(async () => {
    setDetail(await getChat(detail.chat.id));
  }, [detail.chat.id, setDetail]);

  // -- Model change --
  const handleModelChange = useCallback(
    (model: string) => {
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
    },
    [detail.chat.id, detail.chat.model, setDetail],
  );

  // -- Cleanup on unmount --
  useEffect(() => {
    return () => {
      streamAbortRef.current?.abort();
      if (autoAttachRetryTimerRef.current) {
        window.clearTimeout(autoAttachRetryTimerRef.current);
      }
      if (reconcileTimerRef.current) {
        window.clearTimeout(reconcileTimerRef.current);
      }
      if (reconcileIntervalRef.current) {
        window.clearInterval(reconcileIntervalRef.current);
      }
      stopReconcileRef.current();
    };
  }, [
    streamAbortRef,
    autoAttachRetryTimerRef,
    reconcileTimerRef,
    reconcileIntervalRef,
    stopReconcileRef,
  ]);

  return {
    nextStreamAbortSignal,
    applyWorkSurfaceStreamEvent,
    resetWorkSurfaceRun,
    ensureStreamingAssistantMessage,
    scheduleAutoAttachRetry,
    attachExistingRunStream,
    startStream,
    queueDeferredInput,
    stopActiveRun,
    resumeActiveRun,
    hydrateWorkSurfaceForChat,
    loadAgentRunProjection,
    refresh,
    handleModelChange,
    streamAbortRef,
    attachedRunRef,
    autoAttachAttemptedRunRef,
    autoAttachRetryTimerRef,
    autoAttachRetryCountsRef,
    reconcileTimerRef,
    reconcileIntervalRef,
    stopReconcileRef,
    runControlMutationRef,
  };
}
