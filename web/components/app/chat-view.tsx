"use client";

import {
  AlertTriangle,
  Bot,
  ClipboardList,
  HardDrive,
  Loader2,
  MessageSquare,
  MoreVertical,
  Monitor,
  RefreshCw,
  Terminal,
  type LucideIcon,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type SetStateAction,
} from "react";
import { ChatActionsMenu } from "@/components/app/chat-actions-menu";
import { ChatDotNavigator } from "@/components/app/chat-dot-navigator";
import { Composer } from "@/components/app/composer";
import { MessageBubble } from "@/components/app/message-bubble";
import { MoveChatModal } from "@/components/app/move-chat-modal";
import {
  WorkSurfacePanel,
  type WorkSurfaceTab,
} from "@/components/app/work-surface-panel";
import { IconButton } from "@/components/ui/icon-button";
import { useChatLifecycleActions } from "@/hooks/use-chat-lifecycle-actions";
import { subscribeChatLifecycleChange } from "@/lib/chat-lifecycle-events";
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
  updateChatWorkspaceSelection,
} from "@/lib/api/chats";
import {
  WebApiError,
  isAuthRequiredError,
  isNotFoundError,
} from "@/lib/api/errors";
import type {
  ChatDetail,
  ChatMessage,
  ComposerOptions,
  EdgeStatusResponse,
  WorkspaceSelection,
} from "@/lib/api/types";
import {
  deriveChatRunUiState,
  isTerminalChatRunStatus,
} from "@/lib/chat-run-state";
import {
  isChatScrolledToBottom,
  shouldAutoScrollChat,
} from "@/lib/chat-scroll-state";
import {
  ACTIVE_AGENT_SURFACE_STATUSES,
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
const RUN_ATTACH_MAX_RETRIES = 4;
const WORKSPACE_SELECTION_STORAGE_KEY = "astra.web.workspaceSelection";
const ATTACHABLE_RUN_STATUSES = new Set([
  "running",
  "blocked",
  "input-queued",
  "waiting",
  "cancelling",
]);

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

function createStreamingAssistantMessage(id: string): ChatMessage {
  return {
    id,
    role: "assistant",
    content: "",
    createdAt: new Date().toISOString(),
    status: "streaming",
    reasoning: "",
    reasoningStatus: "streaming",
  };
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
  const messageIndex = messages.length - 1 - index;
  return messages.map((message, currentIndex) =>
    currentIndex === messageIndex
      ? {
          ...message,
          content: message.content.trim() ? message.content : "Stopped.",
          status: "complete" as const,
          reasoningStatus: "complete" as const,
        }
      : message,
  );
}

function canAttachRunStream(status: string | undefined | null) {
  return status ? ATTACHABLE_RUN_STATUSES.has(status) : false;
}

function defaultWorkspaceSelection(): WorkspaceSelection | null {
  return null;
}

type WorkspaceSelectionState = {
  selection: WorkspaceSelection | null;
  explicit: boolean;
};

function workspaceSelectionStorageKey(chatId: string) {
  return `${WORKSPACE_SELECTION_STORAGE_KEY}.${chatId}`;
}

function defaultWorkspaceSelectionState(): WorkspaceSelectionState {
  return { selection: defaultWorkspaceSelection(), explicit: false };
}

function readStoredWorkspaceSelectionState(
  chatId: string,
): WorkspaceSelectionState {
  if (typeof window === "undefined") {
    return defaultWorkspaceSelectionState();
  }
  const raw = window.localStorage.getItem(workspaceSelectionStorageKey(chatId));
  if (!raw) {
    return defaultWorkspaceSelectionState();
  }
  try {
    const value = JSON.parse(raw) as WorkspaceSelection;
    if (value.kind === "server_sandbox") {
      return { selection: value, explicit: true };
    }
    if (
      value.kind === "edge_workspace" &&
      value.edgeAgentId?.trim() &&
      value.cwd?.trim()
    ) {
      return { selection: value, explicit: true };
    }
  } catch {
    // Fall through to the implicit server sandbox display default.
  }
  return defaultWorkspaceSelectionState();
}

function workspaceSelectionStateFromDetail(
  detail: ChatDetail,
): WorkspaceSelectionState {
  if (
    detail.workspaceSelection &&
    detail.workspaceSelectionExplicit !== false
  ) {
    return { selection: detail.workspaceSelection, explicit: true };
  }
  return readStoredWorkspaceSelectionState(detail.chat.id);
}

function storeWorkspaceSelectionState(
  chatId: string,
  state: WorkspaceSelectionState,
) {
  if (!state.explicit || !state.selection) {
    window.localStorage.removeItem(workspaceSelectionStorageKey(chatId));
    return;
  }
  window.localStorage.setItem(
    workspaceSelectionStorageKey(chatId),
    JSON.stringify(state.selection),
  );
}

function createAssistantPatchController(params: {
  setDetail: Dispatch<SetStateAction<ChatDetail>>;
  getAssistantId: () => string;
}) {
  let framePatch: Partial<ChatMessage> | null = null;
  let frameRaf: number | null = null;
  let mounted = true;

  const applyPatch = (assistantId: string, patch: Partial<ChatMessage>) => {
    if (!mounted) return;
    params.setDetail((current) => ({
      ...current,
      messages: current.messages.map((message) =>
        message.id === assistantId ? { ...message, ...patch } : message,
      ),
    }));
  };

  const flush = () => {
    const patch = framePatch;
    const assistantId = params.getAssistantId();
    framePatch = null;
    frameRaf = null;
    if (patch) {
      applyPatch(assistantId, patch);
    }
  };

  return {
    patchNow(patch: Partial<ChatMessage>) {
      applyPatch(params.getAssistantId(), patch);
    },
    patchBatched(patch: Partial<ChatMessage>) {
      if (!mounted) return;
      framePatch = { ...framePatch, ...patch };
      if (frameRaf === null) {
        frameRaf = requestAnimationFrame(flush);
      }
    },
    flushNow() {
      if (frameRaf !== null) {
        cancelAnimationFrame(frameRaf);
        frameRaf = null;
      }
      flush();
    },
    cancel() {
      mounted = false;
      if (frameRaf !== null) {
        cancelAnimationFrame(frameRaf);
        frameRaf = null;
      }
      framePatch = null;
    },
  };
}

export function ChatView({ initial }: { initial: ChatDetail }) {
  const router = useRouter();
  const [detail, setDetail] = useState(initial);
  const [moveOpen, setMoveOpen] = useState(false);
  const [startingRun, setStartingRun] = useState(false);
  const [queueingDeferredInput, setQueueingDeferredInput] = useState(false);
  const [resumingRun, setResumingRun] = useState(false);
  const [stoppingRun, setStoppingRun] = useState(false);
  const [runAttachRetrySignal, setRunAttachRetrySignal] = useState(0);
  const [workSurface, setWorkSurface] = useState(() =>
    createEmptyWorkSurface(
      initial.session?.backendSessionId ?? null,
      initial.activeRun?.runId ?? null,
    ),
  );
  const [workSurfaceTab, setWorkSurfaceTab] = useState<WorkSurfaceTab>("tasks");
  const [workSurfaceOpenSignal, setWorkSurfaceOpenSignal] = useState(0);
  const [workspaceSelectionState, setWorkspaceSelectionState] =
    useState<WorkspaceSelectionState>(() =>
      workspaceSelectionStateFromDetail(initial),
    );
  const workspaceSelection = workspaceSelectionState.selection;
  const [edgeWorkspaces, setEdgeWorkspaces] = useState<
    EdgeStatusResponse["edges"]
  >([]);
  const [edgeWorkspacesLoading, setEdgeWorkspacesLoading] = useState(false);
  const [edgeWorkspacesError, setEdgeWorkspacesError] = useState<string | null>(
    null,
  );
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const pinnedRef = useRef(true);
  const pendingStartedRef = useRef<string | null>(null);
  const streamAbortRef = useRef<AbortController | null>(null);
  const workspaceSelectionRequestRef = useRef(0);
  const attachedRunRef = useRef<string | null>(null);
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
  const lifecycle = useChatLifecycleActions({ onChatUpdated: setDetail });
  const { addToast } = useToast();

  const latestMessage = detail.messages[detail.messages.length - 1];
  const isArchived = Boolean(detail.chat.archivedAt);
  const chatListHref = detail.chat.projectId
    ? `/projects/${detail.chat.projectId}`
    : "/chats";
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
  const showRunStatusPanel = Boolean(detail.activeRun?.runId && canResumeRun);
  const agentActivityCount = runActivityMetricCount(
    workSurface.agents.filter((agent) =>
      ACTIVE_AGENT_SURFACE_STATUSES.has(agent.status),
    ).length,
    workSurface.agents.length,
  );
  const toolActivityCount = runActivityMetricCount(
    workSurface.tools.filter((tool) => tool.status === "running").length,
    workSurface.tools.length,
  );
  const openTaskCount = workSurface.tasks.filter((task) =>
    ["in_progress", "pending", "paused"].includes(task.status),
  ).length;
  const taskActivityCount = runActivityMetricCount(
    openTaskCount,
    workSurface.tasks.length,
  );
  const activeRunIsVisibleActivity = Boolean(
    detail.activeRun?.runId &&
    activeRunStatus &&
    !isTerminalChatRunStatus(activeRunStatus),
  );
  const workSurfaceActiveRun = activeRunIsVisibleActivity
    ? detail.activeRun
    : undefined;
  const showRunActivityBar = Boolean(
    !isArchived && !canResumeRun && (startingRun || activeRunIsVisibleActivity),
  );
  const runActivityLabel =
    stoppingRun || activeRunStatus === "cancelling"
      ? "Stopping"
      : startingRun
        ? "Starting run"
        : activeRunLabel;
  const workspaceSelectorDisabled = Boolean(
    startingRun ||
    composerDisabled ||
    (detail.activeRun?.runId &&
      activeRunStatus &&
      !isTerminalChatRunStatus(activeRunStatus)),
  );

  const setWorkspaceSelection = useCallback(
    (selection: WorkspaceSelection) => {
      const previous = workspaceSelectionState;
      const next = { selection, explicit: true };
      const requestId = workspaceSelectionRequestRef.current + 1;
      workspaceSelectionRequestRef.current = requestId;
      setWorkspaceSelectionState(next);
      storeWorkspaceSelectionState(detail.chat.id, next);
      void updateChatWorkspaceSelection(detail.chat.id, selection)
        .then((updated) => {
          if (workspaceSelectionRequestRef.current !== requestId) {
            return;
          }
          const updatedState = workspaceSelectionStateFromDetail(updated);
          setDetail(updated);
          setWorkspaceSelectionState(updatedState);
          storeWorkspaceSelectionState(updated.chat.id, updatedState);
        })
        .catch((error) => {
          if (workspaceSelectionRequestRef.current !== requestId) {
            return;
          }
          setWorkspaceSelectionState(previous);
          storeWorkspaceSelectionState(detail.chat.id, previous);
          addToast(
            `Workspace was not updated. ${
              error instanceof Error
                ? error.message
                : "Failed to persist workspace selection."
            }`,
            "warning",
          );
        });
    },
    [addToast, detail.chat.id, workspaceSelectionState],
  );

  const refreshEdgeWorkspaces = useCallback(async () => {
    setEdgeWorkspacesLoading(true);
    setEdgeWorkspacesError(null);
    try {
      const status = await getEdgeStatus();
      setEdgeWorkspaces(status.edges);
    } catch (error) {
      setEdgeWorkspacesError(
        error instanceof Error
          ? error.message
          : "Failed to load edge workspaces.",
      );
    } finally {
      setEdgeWorkspacesLoading(false);
    }
  }, []);

  const nextStreamAbortSignal = useCallback(() => {
    streamAbortRef.current?.abort();
    const controller = new AbortController();
    streamAbortRef.current = controller;
    return controller.signal;
  }, []);

  const applyWorkSurfaceStreamEvent = useCallback(
    (event: Record<string, unknown>) => {
      setWorkSurface((current) => applyWorkSurfaceEvent(current, event));
    },
    [],
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
    [detail.session?.backendSessionId],
  );

  const ensureStreamingAssistantMessage = useCallback(() => {
    const existingAssistantMessageId = findStreamingAssistantMessageId(
      detail.messages,
    );
    if (existingAssistantMessageId) {
      return existingAssistantMessageId;
    }

    const assistantMessageId = crypto.randomUUID();
    setDetail((current) =>
      findStreamingAssistantMessageId(current.messages)
        ? current
        : {
            ...current,
            messages: [
              ...current.messages,
              createStreamingAssistantMessage(assistantMessageId),
            ],
          },
    );
    return assistantMessageId;
  }, [detail.messages]);

  const scheduleAutoAttachRetry = useCallback((runId: string) => {
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
      setRunAttachRetrySignal((signal) => signal + 1);
    }, delayMs);
    return true;
  }, []);

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

      attachedRunRef.current = runId;
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

      void streamExistingChatRun(detail.chat.id, runId, {
        signal: nextStreamAbortSignal(),
        onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
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
            reasoningStatus: "complete",
            status: "complete",
          });
        },
        onCancelled: (content) => {
          assistantPatcher.flushNow();
          clearActiveRun();
          assistantPatcher.patchNow({
            content: content || "Stopped.",
            reasoningStatus: "complete",
            status: "complete",
          });
        },
        onPaused: (content) => {
          assistantPatcher.flushNow();
          assistantPatcher.patchNow({
            content,
            status: "streaming",
          });
        },
      })
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
            reasoningStatus: "complete",
            status: "failed",
          });
        })
        .finally(() => {
          if (attachedRunRef.current === runId) {
            attachedRunRef.current = null;
          }
          assistantPatcher.cancel();
        });
    },
    [
      applyWorkSurfaceStreamEvent,
      chatListHref,
      detail.chat.id,
      nextStreamAbortSignal,
      resetWorkSurfaceRun,
      router,
    ],
  );

  const openWorkSurfaceTab = useCallback((tab: WorkSurfaceTab) => {
    setWorkSurfaceTab(tab);
    setWorkSurfaceOpenSignal((signal) => signal + 1);
  }, []);

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
              : "Failed to load work surface.",
          ),
        );
      }
    },
    [detail.chat.id, detail.session?.backendSessionId],
  );

  const loadAgentRunProjection = useCallback(
    (runId: string) => getChatWorkSurfaceRun(detail.chat.id, runId),
    [detail.chat.id],
  );

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
      const canReconcilePersistedTranscript = Boolean(pendingMessageId);
      const stopReconcile = () => {
        reconcileTimerRef.current = undefined;
        reconcileIntervalRef.current = undefined;
        stopReconcileRef.current();
      };
      const reconcilePersistedTranscript = async () => {
        if (!canReconcilePersistedTranscript || recoveredFromHydration) {
          return;
        }
        // The SSE proxy writes to the web-store asynchronously; an early poll
        // can observe stale data, so the interval below retries until hydrated.
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
        ...(workspaceSelectionState.explicit && workspaceSelection
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
            attachedRunRef.current = runId;
            resetWorkSurfaceRun(runId);
            setStartingRun(false);
            setDetail((current) => ({
              ...current,
              activeRun: {
                runId,
                status: "running",
                waitingFor: null,
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
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onPaused: (content) => {
            assistantPatcher.flushNow();
            assistantPatcher.patchNow({
              content,
              status: "streaming",
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
            reasoningStatus: "complete",
            status: "failed",
          });
          return;
        }
        const message =
          error instanceof Error ? error.message : "Astra stream failed.";
        assistantPatcher.patchNow({
          content: `I could not reach the Astra runtime from the web UI. (${message})`,
          status: "failed",
        });
      } finally {
        if (streamRunId && attachedRunRef.current === streamRunId) {
          attachedRunRef.current = null;
        }
        assistantPatcher.cancel();
        stopReconcile();
        setStartingRun(false);
      }
    },
    [
      applyWorkSurfaceStreamEvent,
      chatListHref,
      detail.chat.archivedAt,
      detail.chat.id,
      nextStreamAbortSignal,
      refreshEdgeWorkspaces,
      router,
      resetWorkSurfaceRun,
      workspaceSelection,
      workspaceSelectionState.explicit,
    ],
  );

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
        const result = await queueChatRunInput(detail.chat.id, {
          content: text,
          options,
        });
        const assistantMessageId = crypto.randomUUID();
        const runId = result.activeRun.runId;
        attachedRunRef.current = runId;
        resetWorkSurfaceRun(runId);
        setDetail((current) => ({
          ...current,
          activeRun: result.activeRun,
          messages: [
            ...current.messages,
            result.userMessage,
            {
              id: assistantMessageId,
              role: "assistant" as const,
              content: "",
              createdAt: new Date().toISOString(),
              status: "streaming" as const,
              reasoning: "",
              reasoningStatus: "streaming" as const,
            },
          ].sort(
            (a, b) =>
              new Date(a.createdAt).getTime() - new Date(b.createdAt).getTime(),
          ),
        }));
        const assistantPatcher = createAssistantPatchController({
          setDetail,
          getAssistantId: () => assistantMessageId,
        });
        void streamExistingChatRun(detail.chat.id, runId, {
          signal: nextStreamAbortSignal(),
          onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
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
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onPaused: (content) => {
            assistantPatcher.flushNow();
            assistantPatcher.patchNow({
              content,
              status: "streaming",
            });
          },
        })
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
              status: "failed",
            });
          })
          .finally(() => {
            if (attachedRunRef.current === runId) {
              attachedRunRef.current = null;
            }
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
      detail.chat.archivedAt,
      detail.chat.id,
      nextStreamAbortSignal,
      router,
      resetWorkSurfaceRun,
      startStream,
    ],
  );

  const stopActiveRun = useCallback(() => {
    if (!detail.activeRun?.runId || !canStopRun) {
      return;
    }
    const runId = detail.activeRun.runId;
    if (runControlMutationRef.current) {
      return;
    }
    runControlMutationRef.current = true;
    setStoppingRun(true);
    streamAbortRef.current?.abort();
    if (attachedRunRef.current === runId) {
      attachedRunRef.current = null;
    }
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

    void stopChatRun(detail.chat.id)
      .then((result) => {
        setDetail((current) => ({
          ...current,
          activeRun: result.activeRun,
        }));
        void hydrateWorkSurfaceForChat({ silent: true });
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
        void getChat(detail.chat.id)
          .then(setDetail)
          .catch(() => {
            // The local stop still took effect; the toast below is the durable
            // signal if both cancellation and refresh fail.
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
    detail.activeRun?.runId,
    detail.chat.id,
    hydrateWorkSurfaceForChat,
    router,
  ]);

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
    try {
      const result = await resumeChatRun(detail.chat.id);
      if (!result.activeRun?.runId) {
        throw new Error("Resume response did not include an active run.");
      }
      const runId = result.activeRun.runId;
      attachedRunRef.current = runId;
      resetWorkSurfaceRun(runId);
      setDetail((current) => ({
        ...current,
        activeRun: result.activeRun,
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
        await streamExistingChatRun(detail.chat.id, runId, {
          signal: nextStreamAbortSignal(),
          onWorkSurfaceEvent: applyWorkSurfaceStreamEvent,
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
              reasoningStatus: "complete",
              status: "complete",
            });
          },
          onPaused: (content) => {
            assistantPatcher.flushNow();
            assistantPatcher.patchNow({
              content,
              status: "streaming",
            });
          },
        });
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
        try {
          const refreshed = await getChat(detail.chat.id);
          setDetail(refreshed);
        } catch {
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
        if (attachedRunRef.current === runId) {
          attachedRunRef.current = null;
        }
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
    detail.activeRun?.runId,
    detail.chat.id,
    detail.messages,
    nextStreamAbortSignal,
    resetWorkSurfaceRun,
    router,
  ]);

  useEffect(
    () => () => {
      streamAbortRef.current?.abort();
      if (autoAttachRetryTimerRef.current) {
        window.clearTimeout(autoAttachRetryTimerRef.current);
        autoAttachRetryTimerRef.current = undefined;
      }
      stopReconcileRef.current();
    },
    [],
  );

  useEffect(() => {
    void hydrateWorkSurfaceForChat();
  }, [hydrateWorkSurfaceForChat]);

  useEffect(() => {
    const nextState =
      detail.workspaceSelection && detail.workspaceSelectionExplicit !== false
        ? { selection: detail.workspaceSelection, explicit: true }
        : readStoredWorkspaceSelectionState(detail.chat.id);
    setWorkspaceSelectionState(nextState);
    storeWorkspaceSelectionState(detail.chat.id, nextState);
  }, [
    detail.chat.id,
    detail.workspaceSelection,
    detail.workspaceSelectionExplicit,
  ]);

  useEffect(() => {
    void refreshEdgeWorkspaces();
  }, [refreshEdgeWorkspaces]);

  useEffect(() => {
    const activeRun = detail.activeRun;
    if (!activeRun?.runId) {
      autoAttachAttemptedRunRef.current = null;
      autoAttachRetryCountsRef.current.clear();
      if (autoAttachRetryTimerRef.current) {
        window.clearTimeout(autoAttachRetryTimerRef.current);
        autoAttachRetryTimerRef.current = undefined;
      }
      return;
    }
    if (
      isArchived ||
      detail.pendingTurn ||
      !canAttachRunStream(activeRun.status) ||
      attachedRunRef.current === activeRun.runId ||
      autoAttachAttemptedRunRef.current === activeRun.runId
    ) {
      return;
    }

    autoAttachAttemptedRunRef.current = activeRun.runId;
    const assistantMessageId = ensureStreamingAssistantMessage();
    attachExistingRunStream(
      activeRun.runId,
      assistantMessageId,
      "The run is active, but the web UI could not reconnect to its stream.",
      {
        scheduleRetry: () => scheduleAutoAttachRetry(activeRun.runId),
      },
    );
  }, [
    attachExistingRunStream,
    detail.activeRun,
    detail.pendingTurn,
    ensureStreamingAssistantMessage,
    isArchived,
    runAttachRetrySignal,
    scheduleAutoAttachRetry,
  ]);

  useEffect(() => {
    if (shouldAutoScrollChat({ pinnedToBottom: pinnedRef.current })) {
      scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
    }
  }, [
    detail.messages.length,
    latestMessage?.content,
    latestMessage?.reasoning,
    latestMessage?.artifacts?.length,
  ]);

  useEffect(() => {
    if (
      isArchived ||
      !detail.pendingTurn ||
      pendingStartedRef.current === detail.pendingTurn.messageId
    ) {
      return;
    }
    const pendingTurn = detail.pendingTurn;
    const timer = window.setTimeout(() => {
      if (pendingStartedRef.current === pendingTurn.messageId) {
        return;
      }
      pendingStartedRef.current = pendingTurn.messageId;
      void startStream({
        text: pendingTurn.content,
        options: pendingTurn.options,
        pendingMessageId: pendingTurn.messageId,
        appendUser: false,
      });
    }, 0);
    return () => window.clearTimeout(timer);
  }, [detail.pendingTurn, isArchived, startStream]);

  async function refresh() {
    setDetail(await getChat(detail.chat.id));
  }

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
    [detail.chat.id, detail.chat.model],
  );

  useEffect(
    () =>
      subscribeChatLifecycleChange((event) => {
        if (event.action === "clearArchived") {
          if (isArchived) {
            router.replace(chatListHref);
          }
          return;
        }
        if (event.chatId !== detail.chat.id) {
          return;
        }
        if (event.action === "delete") {
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
            addToast(
              error instanceof Error
                ? error.message
                : "Failed to refresh chat state.",
              "error",
            );
          });
      }),
    [addToast, chatListHref, detail.chat.id, isArchived, router],
  );

  return (
    <div className="astra-chat-view relative flex h-full min-h-0 overflow-hidden bg-bg">
      <div className="relative flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
        <header className="relative z-10 flex min-h-[58px] shrink-0 items-center gap-4 border-b border-border/60 bg-bg/85 px-7 backdrop-blur">
          <Link
            href={
              detail.chat.projectId
                ? `/projects/${detail.chat.projectId}`
                : "/chats"
            }
            className="inline-flex items-center gap-1 text-[13px] text-text-muted transition-colors hover:text-text"
          >
            ← {detail.project?.name ?? "Chats"}
          </Link>
          <div className="min-w-0">
            <h1 className="truncate text-sm font-semibold tracking-[-0.01em]">
              {detail.chat.title ?? "Untitled"}
            </h1>
            <div className="mt-0.5 flex items-center gap-1.5 text-xs text-text-muted">
              <span className="size-1.5 rounded-full bg-success" />
              <span>{detail.chat.model ?? "Default model"}</span>
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
            trigger={
              <IconButton
                icon={MoreVertical}
                label="Chat menu"
                className="size-8"
              />
            }
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
          <div className="mx-auto w-full max-w-[860px] px-7 pb-44 pt-10">
            {detail.messages.map((message, index) => (
              <div key={message.id} data-chat-message-index={index}>
                <MessageBubble message={message} />
              </div>
            ))}
          </div>
        </div>
        <ChatDotNavigator
          messages={detail.messages}
          scrollContainerRef={scrollRef}
        />

        <div className="pointer-events-none absolute inset-x-0 bottom-0 z-10 bg-gradient-to-t from-bg via-bg/95 to-bg/0 px-7 pb-6 pt-12">
          <div className="pointer-events-auto mx-auto w-full max-w-[860px]">
            {isArchived ? (
              <div className="rounded-[20px] border border-border bg-surface px-5 py-4 shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.06),0_0_0_0.5px_rgba(120,113,108,0.18)]">
                <div className="flex items-center justify-between gap-4">
                  <div className="min-w-0">
                    <p className="text-sm font-medium text-text">
                      This chat is archived.
                    </p>
                    <p className="mt-1 text-sm text-text-muted">
                      Archived chats are read-only. Unarchive it to continue.
                    </p>
                  </div>
                  <button
                    type="button"
                    disabled={lifecycle.busyChatId === detail.chat.id}
                    onClick={() => {
                      void lifecycle.unarchive(detail.chat.id);
                    }}
                    className="shrink-0 rounded-control bg-text px-3 py-2 text-sm font-medium text-white hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    Unarchive
                  </button>
                </div>
              </div>
            ) : (
              <>
                {showRunActivityBar ? (
                  <RunActivityBar
                    label={runActivityLabel}
                    agents={agentActivityCount}
                    tools={toolActivityCount}
                    tasks={taskActivityCount}
                    onOpenAgents={() => openWorkSurfaceTab("agents")}
                    onOpenTasks={() => openWorkSurfaceTab("tasks")}
                    onOpenTools={() => openWorkSurfaceTab("tools")}
                  />
                ) : null}
                <WorkspaceSelector
                  selection={workspaceSelection}
                  explicit={workspaceSelectionState.explicit}
                  edges={edgeWorkspaces}
                  loading={edgeWorkspacesLoading}
                  error={edgeWorkspacesError}
                  disabled={workspaceSelectorDisabled}
                  onRefresh={refreshEdgeWorkspaces}
                  onSelect={setWorkspaceSelection}
                />
                <Composer
                  disabled={composerDisabled}
                  placeholder={composerPlaceholder}
                  initialModel={
                    detail.pendingTurn?.options.model ??
                    detail.chat.model ??
                    undefined
                  }
                  persistModelPreference={false}
                  onModelChange={handleModelChange}
                  showStop={Boolean(canStopRun && canQueueDeferredInput)}
                  stopping={stoppingRun}
                  stopDisabled={runControlBusy}
                  onStop={() => {
                    void stopActiveRun();
                  }}
                  projectContext={
                    detail.chat.projectId
                      ? { projectId: detail.chat.projectId }
                      : undefined
                  }
                  onSubmit={async ({ text, options }) => {
                    if (canQueueDeferredInput) {
                      await queueDeferredInput({ text, options });
                      return;
                    }
                    void startStream({ text, options, appendUser: true });
                  }}
                />
                {showRunStatusPanel ? (
                  <div className="mt-3 flex items-center justify-between gap-3 rounded-[16px] border border-border/70 bg-surface px-4 py-3 text-sm text-text-muted">
                    <p>
                      {activeRunStatus === "paused"
                        ? "This run is paused. Resume to continue or Stop to cancel it."
                        : activeRunBlocksNewInput
                          ? `Run status is ${activeRunStatus}. Stop it or refresh before sending new input.`
                          : "Stopping current run"}
                    </p>
                    <div className="flex shrink-0 items-center gap-2">
                      {canResumeRun ? (
                        <button
                          type="button"
                          onClick={() => {
                            void resumeActiveRun();
                          }}
                          disabled={runControlBusy}
                          className="rounded-control bg-text px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                        >
                          {resumingRun ? "Resuming..." : "Resume"}
                        </button>
                      ) : null}
                      {canStopRun ? (
                        <button
                          type="button"
                          onClick={() => {
                            void stopActiveRun();
                          }}
                          disabled={runControlBusy}
                          className="rounded-control border border-border bg-bg px-3 py-2 text-sm font-medium text-text transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50"
                        >
                          {stoppingRun ? "Stopping..." : "Stop"}
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
      <WorkSurfacePanel
        state={workSurface}
        activeRun={workSurfaceActiveRun}
        tab={workSurfaceTab}
        onTabChange={setWorkSurfaceTab}
        openSignal={workSurfaceOpenSignal}
        onRefresh={() => {
          void hydrateWorkSurfaceForChat();
        }}
        onLoadAgentRun={loadAgentRunProjection}
      />
    </div>
  );
}

function WorkspaceSelector({
  selection,
  explicit,
  edges,
  loading,
  error,
  disabled,
  onRefresh,
  onSelect,
}: {
  selection: WorkspaceSelection | null;
  explicit: boolean;
  edges: EdgeStatusResponse["edges"];
  loading: boolean;
  error: string | null;
  disabled: boolean;
  onRefresh: () => void | Promise<void>;
  onSelect: (selection: WorkspaceSelection) => void;
}) {
  const edgeOptions = edges
    .filter((edge) => edge.workspace_dir?.trim())
    .map((edge) => ({
      edgeAgentId: edge.edge_agent_id,
      displayName: edge.hostname ?? edge.edge_agent_id,
      cwd: edge.workspace_dir!,
      connectedSecs: edge.connected_secs,
    }));
  const selectedEdge =
    selection?.kind === "edge_workspace"
      ? edgeOptions.find(
          (edge) =>
            edge.edgeAgentId === selection.edgeAgentId &&
            edge.cwd === selection.cwd,
        )
      : null;
  const selectedEdgeMissing =
    explicit && selection?.kind === "edge_workspace" && !selectedEdge;
  const selectedOfflineLabel =
    selection?.kind === "edge_workspace"
      ? `${selection.displayName ?? selection.edgeAgentId} · ${selection.cwd}`
      : "";

  return (
    <div className="mb-2 flex flex-wrap items-center gap-2 rounded-[14px] border border-border/70 bg-surface/95 px-3 py-2 text-xs text-text-muted shadow-[0_0.15rem_0.8rem_rgba(28,25,23,0.05)]">
      <span className="font-medium text-text">Workspace</span>
      {!explicit || !selection ? (
        <span className="inline-flex min-w-0 items-center gap-1.5 rounded-full bg-bg px-2.5 py-1.5 font-medium text-text-secondary">
          <MessageSquare className="size-3.5 shrink-0 text-text-muted" />
          <span className="truncate">Chat only</span>
          <span className="hidden border-l border-border pl-1.5 font-normal text-text-muted sm:inline">
            No code workspace
          </span>
        </span>
      ) : null}
      <button
        type="button"
        disabled={disabled}
        onClick={() => onSelect({ kind: "server_sandbox" })}
        className={[
          "inline-flex max-w-full items-center gap-1.5 rounded-full px-2.5 py-1.5 font-medium transition focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-not-allowed disabled:opacity-50",
          explicit && selection?.kind === "server_sandbox"
            ? "bg-text text-white"
            : "bg-bg text-text-secondary hover:bg-surface-muted hover:text-text",
        ].join(" ")}
      >
        <HardDrive className="size-3.5 shrink-0" />
        <span className="truncate">Server sandbox</span>
      </button>
      {edgeOptions.map((edge) => {
        const selected =
          explicit &&
          selection?.kind === "edge_workspace" &&
          selection.edgeAgentId === edge.edgeAgentId &&
          selection.cwd === edge.cwd;
        return (
          <button
            key={`${edge.edgeAgentId}:${edge.cwd}`}
            type="button"
            disabled={disabled}
            title={`${edge.displayName} · ${edge.cwd}`}
            onClick={() =>
              onSelect({
                kind: "edge_workspace",
                edgeAgentId: edge.edgeAgentId,
                displayName: edge.displayName,
                cwd: edge.cwd,
              })
            }
            className={[
              "inline-flex min-w-0 max-w-[min(30rem,100%)] items-center gap-1.5 rounded-full px-2.5 py-1.5 font-medium transition focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-not-allowed disabled:opacity-50",
              selected
                ? "bg-text text-white"
                : "bg-bg text-text-secondary hover:bg-surface-muted hover:text-text",
            ].join(" ")}
          >
            <Monitor className="size-3.5 shrink-0" />
            <span className="truncate">{edge.displayName}</span>
            <span
              className={[
                "min-w-0 truncate border-l pl-1.5 font-normal",
                selected
                  ? "border-white/30 text-white/75"
                  : "border-border text-text-muted",
              ].join(" ")}
            >
              {edge.cwd}
            </span>
            <span className="sr-only">
              connected for {edge.connectedSecs} seconds
            </span>
          </button>
        );
      })}
      <button
        type="button"
        disabled={loading}
        onClick={() => {
          void onRefresh();
        }}
        className="inline-flex size-7 items-center justify-center rounded-full bg-bg text-text-muted transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-wait disabled:opacity-60"
        aria-label="Refresh edge workspaces"
        title="Refresh edge workspaces"
      >
        <RefreshCw
          className={["size-3.5", loading ? "animate-spin" : ""].join(" ")}
        />
      </button>
      {edgeOptions.length === 0 && !loading ? (
        <span className="text-text-muted">No edge workspaces online</span>
      ) : null}
      {selectedEdgeMissing ? (
        <div
          className="flex min-w-0 max-w-full flex-wrap items-center gap-2 rounded-[10px] border border-warning/30 bg-warning/10 px-2.5 py-1.5 text-warning"
          role="status"
          aria-label={`Selected edge workspace is offline: ${selectedOfflineLabel}`}
        >
          <AlertTriangle className="size-3.5 shrink-0" />
          <span className="min-w-0 max-w-[min(28rem,100%)] truncate">
            Edge offline · {selectedOfflineLabel}
          </span>
          <button
            type="button"
            disabled={disabled}
            onClick={() => onSelect({ kind: "server_sandbox" })}
            className="rounded-full bg-bg px-2 py-0.5 font-medium text-text transition hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50"
          >
            Use server sandbox
          </button>
        </div>
      ) : null}
      {error ? (
        <span className="max-w-full truncate text-warning">{error}</span>
      ) : null}
    </div>
  );
}

function RunActivityBar({
  label,
  agents,
  tools,
  tasks,
  onOpenAgents,
  onOpenTasks,
  onOpenTools,
}: {
  label: string;
  agents: RunActivityMetricCount;
  tools: RunActivityMetricCount;
  tasks: RunActivityMetricCount;
  onOpenAgents: () => void;
  onOpenTasks: () => void;
  onOpenTools: () => void;
}) {
  return (
    <div className="mb-3 flex flex-wrap items-center gap-2 rounded-[14px] border border-border/70 bg-surface/95 px-3 py-2 text-xs text-text-muted shadow-[0_0.15rem_0.8rem_rgba(28,25,23,0.05)]">
      <span className="inline-flex min-w-0 items-center gap-2 font-medium text-text">
        <Loader2 className="size-3.5 animate-spin text-accent" />
        <span className="truncate">{label}</span>
      </span>
      <span
        className="inline-flex items-center gap-1 pl-0.5"
        aria-hidden="true"
      >
        <span className="size-1.5 animate-bounce rounded-full bg-text-muted" />
        <span
          className="size-1.5 animate-bounce rounded-full bg-text-muted"
          style={{ animationDelay: "120ms" }}
        />
        <span
          className="size-1.5 animate-bounce rounded-full bg-text-muted"
          style={{ animationDelay: "240ms" }}
        />
      </span>
      <span className="h-4 w-px bg-border" aria-hidden="true" />
      <RunActivityMetric
        icon={Bot}
        label="Agents"
        value={agents}
        onClick={onOpenAgents}
      />
      <RunActivityMetric
        icon={ClipboardList}
        label="Tasks"
        value={tasks}
        onClick={onOpenTasks}
      />
      <RunActivityMetric
        icon={Terminal}
        label="Tools"
        value={tools}
        onClick={onOpenTools}
      />
    </div>
  );
}

function RunActivityMetric({
  icon: Icon,
  label,
  value,
  onClick,
}: {
  icon: LucideIcon;
  label: string;
  value: RunActivityMetricCount;
  onClick: () => void;
}) {
  const unitLabel = value.mode === "active" ? "active" : "item";
  const pluralUnit =
    value.mode === "active" || value.value === 1 ? unitLabel : `${unitLabel}s`;
  const title =
    value.mode === "active"
      ? `Open ${label} (${value.active} active, ${value.total} total)`
      : `Open ${label} (${value.total} total)`;
  return (
    <button
      type="button"
      className="inline-flex min-w-0 items-center gap-1.5 rounded-full bg-bg px-2.5 py-1 font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-accent/30"
      onClick={onClick}
      aria-label={`Open ${label.toLowerCase()} work surface, ${value.value} ${pluralUnit}`}
      title={title}
    >
      <Icon className="size-3.5 text-text-muted" />
      <span className="tabular-nums text-text">{value.value}</span>
      <span className="hidden sm:inline">{label}</span>
      <span className="hidden text-text-muted xl:inline">{pluralUnit}</span>
    </button>
  );
}

type RunActivityMetricCount = {
  active: number;
  total: number;
  value: number;
  mode: "active" | "item";
};

function runActivityMetricCount(
  active: number,
  total: number,
): RunActivityMetricCount {
  if (active > 0) {
    return { active, total, value: active, mode: "active" };
  }
  return { active, total, value: total, mode: "item" };
}
