"use client";

import { useRouter } from "next/navigation";
import { appendExplainFact, markExplainGap, beginExplainRepair, finishExplainRepair } from "@/lib/explain-analyze-observation";
import {
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
  type Dispatch,
  type SetStateAction,
} from "react";
import {
  getChat,
  getEdgeStatus,
  getChatWorkSurface,
  getChatWorkSurfaceRun,
  queueChatRunInput,
  respondChatRunApproval,
  resumeChatRun,
  stopChatRun,
  streamChatMessage,
  streamExistingChatRun,
  updateChatModel,
  type PendingRunApproval,
} from "@/lib/api/chats";
import { appendRunFailure } from "@/lib/api/stream-event-helpers";
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
  type WorkSurfaceEvent,
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

type StreamBinding = {
  chatId: string;
  sessionId: string | null;
  runId: string;
  assistantMessageId: string;
};

function streamBinding(detail: ChatDetail): StreamBinding | null {
  const run = detail.activeRun;
  return run?.runId && run.assistantMessageId ? {
    chatId: detail.chat.id,
    sessionId: detail.session?.backendSessionId ?? null,
    runId: run.runId,
    assistantMessageId: run.assistantMessageId,
  } : null;
}

function sameStreamBinding(left: StreamBinding | null, right: StreamBinding | null) {
  return left === right || !!left && !!right && left.chatId === right.chatId &&
    left.sessionId === right.sessionId && left.runId === right.runId &&
    left.assistantMessageId === right.assistantMessageId;
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
  runControlBusy: boolean;
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
  pendingApproval: PendingRunApproval | null;
  approvalResponsePending: boolean;
  respondToApproval: (decision: "allow" | "deny") => Promise<void>;
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
    runControlBusy,
    setStartingRun,
    setResumingRun,
    setStoppingRun,
    setQueueingDeferredInput,
    setWorkSurface,
    runAttachRetrySignal,
    setRunAttachRetrySignal: _setRunAttachRetrySignal,
    refreshEdgeWorkspaces,
  } = params;

  // Refs
  const streamAbortRef = useRef<AbortController | null>(null);
  const attachedRunRef = useRef<StreamBinding | null>(null);
  const attachedRunLeaseRef = useRef(0);
  const autoAttachAttemptedRunRef = useRef<StreamBinding | null>(null);
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
  const detailRef = useRef(detail);
  detailRef.current = detail;
  const [pendingApproval, setPendingApproval] = useState<PendingRunApproval | null>(null);
  const [approvalResponsePending, setApprovalResponsePending] = useState(false);
  const handleApprovalRequired = useCallback((approval: PendingRunApproval) => {
    setPendingApproval(approval);
  }, []);
  const clearPendingInteraction = useCallback(() => {
    setPendingApproval(null);
  }, []);
  const workSurfaceRepairInFlightRef = useRef(false);
  const workSurfaceRepairPendingRef = useRef(false);
  const explainGapRepairRef = useRef(
    new Map<
      string,
      { assistantMessageId: string; nextEventIndex: number; pending: boolean }
    >(),
  );

  // -- Stream signal helpers --
  const nextStreamAbortController = useCallback(() => {
    streamAbortRef.current?.abort();
    attachedRunLeaseRef.current += 1;
    attachedRunRef.current = null;
    const controller = new AbortController();
    streamAbortRef.current = controller;
    return controller;
  }, []);

  const claimAttachedRun = useCallback((binding: StreamBinding) => {
    attachedRunRef.current = binding;
    return attachedRunLeaseRef.current;
  }, []);

  const clearAttachedRun = useCallback((runId?: string, lease?: number) => {
    if (runId && attachedRunRef.current?.runId !== runId) return;
    if (lease !== undefined && attachedRunLeaseRef.current !== lease) return;
    if (lease === undefined) {
      attachedRunLeaseRef.current += 1;
      streamAbortRef.current?.abort();
    }
    attachedRunRef.current = null;
    streamAbortRef.current = null;
  }, []);

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

  const scheduleAutoAttachRetry = useCallback((binding: StreamBinding) => {
    const key = JSON.stringify(binding);
    const attempts = autoAttachRetryCountsRef.current.get(key) ?? 0;
    if (attempts >= RUN_ATTACH_MAX_RETRIES) return false;
    autoAttachRetryCountsRef.current.set(key, attempts + 1);
    if (autoAttachRetryTimerRef.current) window.clearTimeout(autoAttachRetryTimerRef.current);
    autoAttachRetryTimerRef.current = window.setTimeout(() => {
      autoAttachRetryTimerRef.current = undefined;
      if (!sameStreamBinding(streamBinding(detailRef.current), binding)) return;
      if (sameStreamBinding(autoAttachAttemptedRunRef.current, binding)) autoAttachAttemptedRunRef.current = null;
      _setRunAttachRetrySignal(signal => signal + 1);
    }, Math.min(1_000 * 2 ** attempts, 8_000));
    return true;
  }, [_setRunAttachRetrySignal]);

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

  const repairWorkSurfaceFromDurable = useCallback(() => {
    if (workSurfaceRepairInFlightRef.current) {
      workSurfaceRepairPendingRef.current = true;
      return;
    }
    workSurfaceRepairInFlightRef.current = true;
    void (async () => {
      try {
        do {
          workSurfaceRepairPendingRef.current = false;
          await hydrateWorkSurfaceForChat({ silent: true });
        } while (workSurfaceRepairPendingRef.current);
      } finally {
        workSurfaceRepairInFlightRef.current = false;
      }
    })();
  }, [hydrateWorkSurfaceForChat]);

  const applyWorkSurfaceStreamEvent = useCallback(
    (event: WorkSurfaceEvent) => {
      setWorkSurface((current) => applyWorkSurfaceEvent(current, event));
      if (
        event.type === "stream_gap" &&
        event.explain_analyze_recovered !== true
      ) {
        setDetail((current) => {
          const activeAssistantId =
            current.activeRun?.runId === event.run_id
              ? current.activeRun.assistantMessageId
              : undefined;
          const fallbackAssistantId = [...current.messages]
            .reverse()
            .find((message) => message.role === "assistant" && message.status === "streaming")
            ?.id;
          const targetId = activeAssistantId ?? fallbackAssistantId;
          if (!targetId) return current;
          return {
            ...current,
            messages: current.messages.map((message) =>
              message.id === targetId
                ? markExplainGap(message, event.explain_analyze_recovered === false)
                : message,
            ),
          };
        });
      }
      if (event.type === "agent_live_gap" || event.type === "stream_gap") {
        repairWorkSurfaceFromDurable();
      }
    },
    [repairWorkSurfaceFromDurable, setDetail, setWorkSurface],
  );

  const applyArtifactPublication = useCallback(
    (assistantMessageId: string, event: import("@astra/sdk").ArtifactPublicationV1, updateDetail = setDetail) => {
      updateDetail((current) => ({ ...current, messages: current.messages.map((message) =>
        message.id === assistantMessageId ? { ...message, artifactPublication: event } : message) }));
    }, [setDetail],
  );

  const applyExplainAnalyzeEvent = useCallback(
    (assistantMessageId: string, event: import("@astra/sdk").ExplainAnalyzeEventV1, updateDetail = setDetail) => {
      updateDetail((current) => ({
        ...current,
        messages: current.messages.map((message) => {
          if (message.id !== assistantMessageId) return message;
          return appendExplainFact(message, event);
        }),
      }));
    },
    [setDetail],
  );

  const markExplainAnalyzeInvalid = useCallback((assistantMessageId: string, updateDetail = setDetail) => {
    updateDetail((current) => ({ ...current, messages: current.messages.map((message) =>
      message.id === assistantMessageId ? markExplainGap(message, true) : message,
    ) }));
  }, [setDetail]);

  const repairExplainAnalyzeFromDurable = useCallback(
    (
      assistantMessageId: string,
      runId: string,
      nextEventIndex: number,
    ) => {
      const existing = explainGapRepairRef.current.get(runId);
      if (existing) {
        existing.assistantMessageId = assistantMessageId;
        existing.nextEventIndex = Math.min(existing.nextEventIndex, nextEventIndex);
        existing.pending = true;
        return;
      }

      const repair = { assistantMessageId, nextEventIndex, pending: false };
      explainGapRepairRef.current.set(runId, repair);
      void (async () => {
        let unrepairedGap = false;
        try {
          do {
            repair.pending = false;
            let pageHasUnrepairedGap = false;
            let sawExplainFact = false;
            const repairToken = crypto.randomUUID();
            setDetail((current) => ({ ...current, messages: current.messages.map((message) =>
              message.id === repair.assistantMessageId ? beginExplainRepair(message, repairToken) : message) }));
            await streamExistingChatRun(
              detail.chat.id,
              runId,
              {
                onArtifactPublication: (event) => applyArtifactPublication(repair.assistantMessageId, event),
                onExplainAnalyzeEvent: (event) => {
                  sawExplainFact = true;
                  applyExplainAnalyzeEvent(repair.assistantMessageId, event);
                },
                onExplainAnalyzeInvalid: () => { pageHasUnrepairedGap = true; markExplainAnalyzeInvalid(repair.assistantMessageId); },
                onWorkSurfaceEvent: (event) => {
                  if (
                    event.type === "stream_gap" &&
                    event.explain_analyze_recovered === false
                  ) {
                    pageHasUnrepairedGap = true;
                    markExplainAnalyzeInvalid(repair.assistantMessageId);
                  }
                },
              },
              {
                assistantMessageId: repair.assistantMessageId,
                // Repair all outstanding delivery gaps for this message. A
                // suffix alone cannot prove earlier missing facts were restored.
                nextEventIndex: 0,
                replayOnly: true,
              },
            );
            unrepairedGap ||= pageHasUnrepairedGap;
            if (sawExplainFact && !unrepairedGap) {
              setDetail((current) => ({ ...current, messages: current.messages.map((message) =>
                message.id === repair.assistantMessageId ? finishExplainRepair(message, repairToken) : message) }));
            }
          } while (repair.pending);

        } catch {
          setDetail((current) => ({
            ...current,
            messages: current.messages.map((message) =>
              message.id === repair.assistantMessageId
                ? markExplainGap(message)
                : message,
            ),
          }));
        } finally {
          explainGapRepairRef.current.delete(runId);
        }
      })();
    },
    [applyArtifactPublication, applyExplainAnalyzeEvent, markExplainAnalyzeInvalid, detail.chat.id, setDetail],
  );

  const loadAgentRunProjection = useCallback(
    (runId: string) => getChatWorkSurfaceRun(detail.chat.id, runId),
    [detail.chat.id],
  );

  // -- Attach existing run stream --
  const handleProjectionFailure = useCallback((
    error: unknown,
    assistantMessageId: string,
    assistantPatcher: ReturnType<typeof createAssistantPatchController>,
    updateDetail = setDetail,
  ): boolean => {
    if (!(error instanceof WebApiError)) return false;
    if (error.code === "stream_projection_busy" || error.code === "stream_continuation_unavailable") {
      assistantPatcher.flushNow();
      addToast(error.detail, "error");
      return true;
    }
    if (error.code !== "canonical_persistence_failed") return false;
    // The HTTP stream reached the runtime. Keep admitted output, including a
    // pending animation-frame patch, instead of treating this as reconnect failure.
    assistantPatcher.flushNow();
    updateDetail((current) => ({
      ...current,
      activeRun: current.activeRun?.assistantMessageId === assistantMessageId ? undefined : current.activeRun,
      messages: current.messages.map((message) => {
        if (message.id !== assistantMessageId) return message;
        const content = appendRunFailure(message.content, error.detail);
        return {
          ...message,
          content,
          completedAt: new Date().toISOString(),
          status: "failed",
          reasoningStatus: undefined,
        };
      }),
    }));
    return true;
  }, [setDetail, addToast]);

  const attachExistingRunStream = useCallback((binding: StreamBinding, nextEventIndex: number) => {
    const { chatId, runId, assistantMessageId } = binding;
    const controller = nextStreamAbortController();
    const signal = controller.signal;
    const lease = claimAttachedRun(binding);
    let receiving = true;
    const ownsSelection = (current: ChatDetail) => attachedRunLeaseRef.current === lease &&
      sameStreamBinding(streamBinding(current), binding);
    const canReceive = () => receiving && !signal.aborted && ownsSelection(detailRef.current);
    // React may execute an update after a replacement was queued in the same batch.
    // EOF closes callbacks, but already-queued updates retain their valid lease.
    const updateDetail: Dispatch<SetStateAction<ChatDetail>> = update => setDetail(current =>
      ownsSelection(current)
        ? typeof update === "function" ? update(current) : update
        : current);
    const assistantPatcher = createAssistantPatchController({ setDetail: updateDetail, getAssistantId: () => assistantMessageId });
    const abort = () => { receiving = false; assistantPatcher.cancel(); };
    signal.addEventListener("abort", abort, { once: true });
    resetWorkSurfaceRun(runId);
    const settleMessage = (content: string, paused = false) => {
      assistantPatcher.flushNow();
      updateDetail(current => ({
        ...current,
        activeRun: paused ? current.activeRun : undefined,
        messages: current.messages.map(message => message.id === assistantMessageId ? {
          ...message,
          ...(content ? { content } : {}),
          completedAt: new Date().toISOString(),
          reasoningStatus: "complete",
          status: "complete",
        } : message),
      }));
    };
    // Immediate layout cleanup (including StrictMode) invalidates this admission.
    void Promise.resolve().then(async () => {
      if (!canReceive()) return;
      await streamExistingChatRun(chatId, runId, {
        signal,
        onWorkSurfaceEvent: event => {
          if (canReceive()) applyWorkSurfaceStreamEvent(event);
        },
        onWarning: message => {
          if (canReceive()) addToast(message, "warning");
        },
        onArtifactPublication: event => {
          if (canReceive()) applyArtifactPublication(assistantMessageId, event, updateDetail);
        },
        onExplainAnalyzeEvent: event => {
          if (canReceive()) applyExplainAnalyzeEvent(assistantMessageId, event, updateDetail);
        },
        onExplainAnalyzeInvalid: () => {
          if (canReceive()) markExplainAnalyzeInvalid(assistantMessageId, updateDetail);
        },
        onStreamGap: gap => {
          if (canReceive()) repairExplainAnalyzeFromDurable(assistantMessageId, gap.runId, gap.nextEventIndex);
        },
        onApprovalRequired: approval => {
          if (canReceive()) handleApprovalRequired(approval);
        },
        onInteractionResolved: () => {
          if (canReceive()) clearPendingInteraction();
        },
        onRunUpdated: run => {
          if (canReceive()) updateDetail(current => ({
            ...current,
            activeRun: mergeStreamRunUpdate({ ...run, assistantMessageId }, current.activeRun),
          }));
        },
        onRunFinished: () => {
          if (canReceive()) setStoppingRun(false);
        },
        onReasoning: reasoning => {
          if (canReceive()) assistantPatcher.patchBatched({ reasoning, reasoningStatus: "streaming", status: "streaming" });
        },
        onReasoningDone: reasoning => {
          if (canReceive()) assistantPatcher.patchBatched({ reasoning, reasoningStatus: "complete", status: "streaming" });
        },
        onText: content => {
          if (canReceive()) assistantPatcher.patchBatched({ content, status: "streaming" });
        },
        onArtifacts: artifacts => {
          if (canReceive()) assistantPatcher.patchBatched({ artifacts });
        },
        onDone: content => {
          if (canReceive()) settleMessage(content || "Astra completed the run without returning visible text.");
        },
        onCancelled: content => {
          if (canReceive()) settleMessage(content || "Stopped.");
        },
        onPaused: content => {
          if (canReceive()) settleMessage(content, true);
        },
      }, { assistantMessageId, nextEventIndex });
    }).catch(async (error: unknown) => {
      if (!canReceive() || isAbortError(error)) return;
      if (isAuthRequiredError(error)) {
        router.push(`/login?next=${encodeURIComponent(`/chats/${chatId}`)}`);
        return;
      }
      if (isNotFoundError(error)) {
        router.replace(chatListHref);
        return;
      }
      if (handleProjectionFailure(error, assistantMessageId, assistantPatcher, updateDetail)) return;
      assistantPatcher.flushNow();
      try {
        const refreshed = await getChat(chatId);
        if (!canReceive()) return;
        updateDetail(refreshed);
      } catch { /* Keep admitted output when authoritative refresh is unavailable. */ }
      if (!canReceive()) return;
      if (!scheduleAutoAttachRetry(binding)) {
        addToast(error instanceof Error ? error.message : "The active run stream could not be reconnected.", "warning");
      }
    }).finally(() => {
      receiving = false;
      signal.removeEventListener("abort", abort);
      clearAttachedRun(runId, lease);
      assistantPatcher.cancel();
    });
  }, [nextStreamAbortController, claimAttachedRun, setDetail, resetWorkSurfaceRun,
    applyWorkSurfaceStreamEvent, applyArtifactPublication, applyExplainAnalyzeEvent, markExplainAnalyzeInvalid,
    addToast, repairExplainAnalyzeFromDurable, handleApprovalRequired,
    clearPendingInteraction, setStoppingRun, router, chatListHref, handleProjectionFailure,
    scheduleAutoAttachRetry, clearAttachedRun]);

  // One committed selection owns live GET admission. Cursor/status progress does
  // not replace an existing transport; a different message/session/root does.
  useLayoutEffect(() => {
    const binding = streamBinding(detail);
    const hasAssistant = !!binding && detail.messages.some(message =>
      message.id === binding.assistantMessageId && message.role === "assistant");
    if (attachedRunRef.current && (!sameStreamBinding(attachedRunRef.current, binding) || !hasAssistant)) clearAttachedRun();
    if (!sameStreamBinding(autoAttachAttemptedRunRef.current, binding)) {
      autoAttachAttemptedRunRef.current = null;
      const selectedKey = binding ? JSON.stringify(binding) : null;
      for (const key of autoAttachRetryCountsRef.current.keys()) {
        if (key !== selectedKey) autoAttachRetryCountsRef.current.delete(key);
      }
      if (autoAttachRetryTimerRef.current) window.clearTimeout(autoAttachRetryTimerRef.current);
      autoAttachRetryTimerRef.current = undefined;
    }
    if (!canAttachRunStream(detail.activeRun?.status)) {
      autoAttachAttemptedRunRef.current = null;
      autoAttachRetryCountsRef.current.clear();
      if (autoAttachRetryTimerRef.current) window.clearTimeout(autoAttachRetryTimerRef.current);
      autoAttachRetryTimerRef.current = undefined;
      return;
    }
    if (isArchived || detail.pendingTurn || runControlBusy || !binding?.sessionId ||
      sameStreamBinding(attachedRunRef.current, binding) ||
      sameStreamBinding(autoAttachAttemptedRunRef.current, binding) ||
      !Number.isSafeInteger(detail.activeRun?.nextEventIndex) || detail.activeRun!.nextEventIndex! < 0 ||
      !hasAssistant) return;
    autoAttachAttemptedRunRef.current = binding;
    attachExistingRunStream(binding, detail.activeRun!.nextEventIndex!);
  }, [detail, isArchived, runControlBusy, runAttachRetrySignal, attachExistingRunStream, clearAttachedRun]);

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
        activeTools: options.activeTools,
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

      const controller = nextStreamAbortController();
      const signal = controller.signal;
      const creationLease = attachedRunLeaseRef.current;
      let streamRunId: string | null = null;
      let streamRunLease: number | null = null;
      let streamSessionId = detail.session?.backendSessionId ?? null;
      const canReceive = () => !signal.aborted && attachedRunLeaseRef.current === creationLease;
      const updateDetail: Dispatch<SetStateAction<ChatDetail>> = update => setDetail(current => {
        if (attachedRunLeaseRef.current !== creationLease || current.chat.id !== detail.chat.id ||
          streamRunId && current.activeRun && (current.activeRun.runId !== streamRunId ||
            current.activeRun.assistantMessageId !== currentAssistantId)) return current;
        return typeof update === "function" ? update(current) : update;
      });
      const assistantPatcher = createAssistantPatchController({
        setDetail: updateDetail,
        getAssistantId: () => currentAssistantId,
      });
      let recoveredFromHydration = false;
      const canReconcilePersistedTranscript = Boolean(pendingMessageId);
      const stopReconcile = () => {
        stopReconcileRef.current();
        reconcileTimerRef.current = undefined;
        reconcileIntervalRef.current = undefined;
      };
      const reconcilePersistedTranscript = async () => {
        if (!canReceive() || !canReconcilePersistedTranscript || recoveredFromHydration) {
          return;
        }
        const refreshed = await getChat(detail.chat.id).catch(() => null);
        if (
          !canReceive() || !refreshed ||
          !hasCompletedAssistantAfterUser(refreshed, pendingMessageId!)
        ) {
          return;
        }
        recoveredFromHydration = true;
        stopReconcile();
        setStartingRun(false);
        setStoppingRun(false);
        updateDetail(refreshed);
        controller.abort();
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
      updateDetail((current) => ({
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
          signal,
          onWorkSurfaceEvent: event => { if (canReceive()) applyWorkSurfaceStreamEvent(event); },
          onWarning: message => { if (canReceive()) addToast(message, "warning"); },
          onArtifactPublication: event => { if (canReceive()) applyArtifactPublication(currentAssistantId, event, updateDetail); },
          onExplainAnalyzeEvent: event => { if (canReceive()) applyExplainAnalyzeEvent(currentAssistantId, event, updateDetail); },
          onExplainAnalyzeInvalid: () => { if (canReceive()) markExplainAnalyzeInvalid(currentAssistantId, updateDetail); },
          onStreamGap: gap => {
            if (canReceive()) repairExplainAnalyzeFromDurable(currentAssistantId, gap.runId, gap.nextEventIndex);
          },
          onApprovalRequired: approval => { if (canReceive()) handleApprovalRequired(approval); },
          onInteractionResolved: () => { if (canReceive()) clearPendingInteraction(); },
          onLocalMessages: ({
            userMessage: localUserMessage,
            assistantMessage: localAssistantMessage,
          }) => {
            if (!canReceive()) return;
            const previousAssistantId = currentAssistantId;
            const previousUserId = currentUserId;
            currentAssistantId = localAssistantMessage.id;
            currentUserId = localUserMessage.id;
            updateDetail((current) => {
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
            if (!canReceive()) return;
            streamRunId = runId;
            streamRunLease = claimAttachedRun({ chatId: detail.chat.id, sessionId: streamSessionId,
              runId, assistantMessageId: currentAssistantId });
            resetWorkSurfaceRun(runId);
            setStartingRun(false);
            updateDetail((current) => ({
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
            if (!canReceive()) return;
            streamSessionId = sessionId;
            if (streamRunLease !== null && attachedRunLeaseRef.current === streamRunLease && attachedRunRef.current) {
              attachedRunRef.current = { ...attachedRunRef.current, sessionId };
            }
            setWorkSurface((current) => ({
              ...resetWorkSurfaceForRun(current, {
                sessionId,
                runId: current.runId,
              }),
              error: null,
            }));
            updateDetail((current) => ({
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
            if (!canReceive()) return;
            updateDetail((current) => ({
              ...current,
              activeRun: mergeStreamRunUpdate(
                { ...run, assistantMessageId: currentAssistantId },
                current.activeRun,
              ),
            }));
          },
          onRunFinished: () => {
            if (!canReceive()) return;
            setStoppingRun(false);
            setStartingRun(false);
          },
          onReasoning: (reasoning) => {
            if (!canReceive()) return;
            assistantPatcher.patchBatched({
              reasoning,
              reasoningStatus: "streaming",
              status: "streaming",
            });
          },
          onReasoningDone: (reasoning) => {
            if (!canReceive()) return;
            assistantPatcher.patchBatched({
              reasoning,
              reasoningStatus: "complete",
              status: "streaming",
            });
          },
          onText: (content) => {
            if (!canReceive()) return;
            assistantPatcher.patchBatched({ content, status: "streaming" });
          },
          onArtifacts: (artifacts) => {
            if (!canReceive()) return;
            assistantPatcher.patchBatched({ artifacts });
          },
          onDone: (content) => {
            if (!canReceive()) return;
            assistantPatcher.flushNow();
            setStartingRun(false);
            updateDetail((current) =>
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
            if (!canReceive()) return;
            assistantPatcher.flushNow();
            setStoppingRun(false);
            setStartingRun(false);
            updateDetail((current) =>
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
            if (!canReceive()) return;
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
        if (!canReceive() || isAbortError(error)) {
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
          updateDetail((current) => ({
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
        if (error instanceof WebApiError && error.code === "stream_observation_interrupted") {
          assistantPatcher.flushNow();
          let observed = detailRef.current;
          try {
            const refreshed = await getChat(detail.chat.id);
            if (!canReceive()) return;
            observed = refreshed;
            updateDetail(refreshed);
          } catch { /* Keep the admitted answer and run when refresh is unavailable. */ }
          if (!canReceive()) return;
          const binding = streamBinding(observed);
          if (!binding || !scheduleAutoAttachRetry(binding)) addToast(error.detail, "warning");
          return;
        }
        if (handleProjectionFailure(error, currentAssistantId, assistantPatcher, updateDetail)) return;
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
        if (attachedRunLeaseRef.current === creationLease) {
          stopReconcile();
          setStartingRun(false);
        }
      }
    },
    [
      addToast,
      applyWorkSurfaceStreamEvent,
      applyArtifactPublication,
    applyExplainAnalyzeEvent,
      markExplainAnalyzeInvalid,
      repairExplainAnalyzeFromDurable,
      clearPendingInteraction,
      handleApprovalRequired,
      handleProjectionFailure,
      scheduleAutoAttachRetry,
      chatListHref,
      claimAttachedRun,
      clearAttachedRun,
      detail.chat.archivedAt,
      detail.chat.id,
      detail.session?.backendSessionId,
      nextStreamAbortController,
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
      if (detail.chat.archivedAt || runControlMutationRef.current) {
        const error = new Error("Input was not queued. Review this session before submitting again.");
        addToast(error.message, "error");
        throw error;
      }
      const matchesRequestedBinding = (current: ChatDetail) => current.chat.id === detail.chat.id &&
        current.session?.backendSessionId === detail.session?.backendSessionId &&
        current.activeRun?.runId === detail.activeRun?.runId &&
        current.activeRun?.assistantMessageId === detail.activeRun?.assistantMessageId;
      runControlMutationRef.current = true;
      setQueueingDeferredInput(true);

      try {
        const pendingMessageId = crypto.randomUUID();
        const result = await queueChatRunInput(detail.chat.id, {
          content: text,
          options,
          pendingMessageId,
        });
        setDetail(current => !matchesRequestedBinding(current) ? current : {
            ...current, activeRun: result.activeRun,
            messages: [...current.messages.filter(message => message.id !== result.userMessage.id && message.id !== result.assistantMessage.id),
              result.userMessage, result.assistantMessage].sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt)),
          });
      } catch (error) {
        if (error instanceof WebApiError && error.status === 409) {
          const refreshed = await getChat(detail.chat.id).catch(() => null);
          if (refreshed?.chat.id === detail.chat.id) {
            setDetail(current => matchesRequestedBinding(current) ? refreshed : current);
          }
        }
        addToast(
          `Queued input could not be confirmed. Your draft was kept. Review this session before submitting again.${error instanceof Error ? ` ${error.message}` : ""}`,
          "error",
        );
        throw error;
      } finally {
        runControlMutationRef.current = false;
        setQueueingDeferredInput(false);
      }
    },
    [addToast, detail, setDetail, setQueueingDeferredInput],
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
    const requestedRunId = detail.activeRun.runId;
    const requestedSessionId = detail.session?.backendSessionId;
    const requestedAssistantMessageId = detail.activeRun.assistantMessageId;
    const sameCurrentRun = (current = detailRef.current) => current.chat.id === detail.chat.id &&
      current.activeRun?.runId === requestedRunId &&
      current.session?.backendSessionId === requestedSessionId &&
      current.activeRun?.assistantMessageId === requestedAssistantMessageId;
    setResumingRun(true);
    try {
      const result = await resumeChatRun(detail.chat.id);
      if (!sameCurrentRun()) return;
      if (result.activeRun?.runId !== requestedRunId) {
        throw new Error("Resume response did not match the requested run.");
      }
      const responseRun = result.activeRun;
      let resumedActiveRun = responseRun;
      let messages = detailRef.current.messages;
      let refreshedDetail: ChatDetail | undefined;
      const hasContinuation = () => !!resumedActiveRun.assistantMessageId &&
        messages.some(message => message.id === resumedActiveRun.assistantMessageId && message.role === "assistant") &&
        Number.isSafeInteger(resumedActiveRun.nextEventIndex) && resumedActiveRun.nextEventIndex! >= 0;
      if (!hasContinuation()) {
        let refreshed: ChatDetail;
        try {
          refreshed = await getChat(detail.chat.id);
        } catch (error) {
          setDetail(current => sameCurrentRun(current) ? { ...current,
            activeRun: { ...responseRun, nextEventIndex: undefined } } : current);
          throw error;
        }
        if (!sameCurrentRun()) return;
        if (refreshed.activeRun?.runId !== requestedRunId ||
          refreshed.session?.backendSessionId !== requestedSessionId) {
          throw new Error("The run changed while restoring its stream binding.");
        }
        refreshedDetail = refreshed;
        resumedActiveRun = refreshed.activeRun;
        messages = refreshed.messages;
      }
      setDetail(current => sameCurrentRun(current)
        ? { ...(refreshedDetail ?? current), activeRun: resumedActiveRun } : current);
      if (!hasContinuation()) {
        throw new WebApiError(409,
          "The run resumed, but its stream continuation is unavailable. Reopen the session to inspect its recorded state.",
          "stream_continuation_unavailable");
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
  }, [addToast, canResumeRun, chatListHref, detail.activeRun?.runId, detail.activeRun?.assistantMessageId,
    detail.chat.id, detail.session?.backendSessionId, setDetail, setResumingRun, router]);

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

  const respondToApproval = useCallback(
    async (decision: "allow" | "deny") => {
      if (!pendingApproval || approvalResponsePending) return;
      const approval = pendingApproval;
      setApprovalResponsePending(true);
      try {
        await respondChatRunApproval(detail.chat.id, approval, decision);
        setPendingApproval((current) =>
          current?.requestId === approval.requestId ? null : current,
        );
      } catch (error) {
        addToast(
          error instanceof Error
            ? error.message
            : "Failed to submit the approval decision.",
          "error",
        );
      } finally {
        setApprovalResponsePending(false);
      }
    },
    [
      addToast,
      approvalResponsePending,
      detail.chat.id,
      pendingApproval,
    ],
  );

  // -- Cleanup on unmount --
  useLayoutEffect(() => {
    const stopReconcile = stopReconcileRef.current;
    const retryCounts = autoAttachRetryCountsRef.current;
    return () => {
      clearAttachedRun();
      autoAttachAttemptedRunRef.current = null;
      retryCounts.clear();
      if (autoAttachRetryTimerRef.current) {
        window.clearTimeout(autoAttachRetryTimerRef.current);
      }
      if (reconcileTimerRef.current) {
        window.clearTimeout(reconcileTimerRef.current);
      }
      if (reconcileIntervalRef.current) {
        window.clearInterval(reconcileIntervalRef.current);
      }
      stopReconcile();
    };
  }, [
    streamAbortRef,
    autoAttachRetryTimerRef,
    reconcileTimerRef,
    reconcileIntervalRef,
    stopReconcileRef,
    clearAttachedRun,
  ]);

  return {
    pendingApproval,
    approvalResponsePending,
    respondToApproval,
    startStream,
    queueDeferredInput,
    stopActiveRun,
    resumeActiveRun,
    hydrateWorkSurfaceForChat,
    loadAgentRunProjection,
    refresh,
    handleModelChange,
  };
}
