import type {
  StreamEvent,
  RunFinishedEvent,
  RunInterruptedEvent,
  StreamErrorEvent,
} from "@astra/sdk";
import { isExplainAnalyzeEventV1, isArtifactPublicationV1 } from "@astra/sdk";
import {
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { mergeTextDelta, splitThinkingTags } from "@/lib/api/stream-text";
import {
  artifactsFromToolCallEnd,
  mergeChatArtifacts,
} from "@/lib/api/stream-artifacts";
import type { ChatArtifactRef } from "@/lib/api/types";
import { projectRunWaitingState } from "@/lib/run-status-messages";
import {
  blockedWaitingFor,
  eventMessage,
  explicitEventMessage,
  isForeignRunLifecycleEvent,
  isRunLifecycleEvent,
} from "@/lib/api/stream-event-helpers";

export interface StreamEventContext {
  ownerUserId: string;
  chatId: string;
  assistantMessageId: string;
  getSessionId: () => string;
}

export interface StreamEventState {
  runId?: string;
  assistantText: string;
  assistantRawText: string;
  lastFailureMessage?: string;
  errorCode?: string;
  reasoningText: string;
  artifacts?: ChatArtifactRef[];
  statusFeedbackText?: string;
  reasoningFromThinkingTags?: boolean;
  lastStatus: "streaming" | "complete" | "failed";
  protocolError: boolean;
  runLifecycle: "running" | "paused" | "waiting" | "blocked" | "finished";
  nextEventIndex?: number;
}

function normalizeEventIndex(value: unknown): number | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    throw new Error("Malformed stream event index.");
  }
  return Math.trunc(value);
}

/** Persist observation facts without replaying transcript or lifecycle mutations. */
export function applyExplainAnalyzeObservation(
  event: StreamEvent,
  ctx: StreamEventContext,
  replayOnly = false,
): void {
  if (event.type === "artifact_publication" && isArtifactPublicationV1(event)) {
    updateStreamingAssistantMessage(ctx.ownerUserId, ctx.chatId, ctx.assistantMessageId, { artifactPublication: event });
  } else if (event.type === "explain_analyze") {
    updateStreamingAssistantMessage(ctx.ownerUserId, ctx.chatId, ctx.assistantMessageId,
      isExplainAnalyzeEventV1(event)
        ? { explainAnalyzeEvent: event }
        : { explainAnalyzeDegraded: true, explainAnalyzeUnrecoverable: true });
  } else if (event.type === "stream_gap" &&
    event.explain_analyze_recovered !== true &&
    (!replayOnly || event.explain_analyze_recovered === false)) {
    // Legacy transport-gap markers can themselves be replayed. Only an explicit
    // unrecoverable Explain boundary proves that a durable repair is incomplete.
    updateStreamingAssistantMessage(ctx.ownerUserId, ctx.chatId, ctx.assistantMessageId,
      { explainAnalyzeDegraded: true,
        ...(event.explain_analyze_recovered === false ? { explainAnalyzeUnrecoverable: true } : {}) });
  }
}

export function applyStreamEvent(
  event: StreamEvent,
  ctx: StreamEventContext,
  state: StreamEventState,
): void {
  const expectedSessionId = ctx.getSessionId();
  const eventIndex = normalizeEventIndex(event.index);
  if (eventIndex !== null) {
    state.nextEventIndex = Math.max(state.nextEventIndex ?? 0, eventIndex + 1);
  }

  if (state.protocolError) {
    return;
  }

  if (isForeignRunLifecycleEvent(event, state.runId)) return;
  if (!state.runId && isRunLifecycleEvent(event.type) &&
    "run_id" in event && typeof event.run_id === "string" && event.run_id.trim()) {
    state.runId = event.run_id;
  }

  const setActiveRun = (run: {
    runId: string;
    status: string;
    waitingFor?: string | null;
  }) => {
    setChatActiveRun(ctx.ownerUserId, ctx.chatId, {
      ...run,
      assistantMessageId: ctx.assistantMessageId,
      nextEventIndex: state.nextEventIndex ?? null,
    });
  };

  const applyAssistantText = (
    rawText: string,
    status: "streaming" | "complete" | "failed",
  ) => {
    const split = splitThinkingTags(rawText);
    state.assistantText = split.visibleText;
    if (split.hasThinking) {
      state.reasoningText = split.reasoning;
      state.reasoningFromThinkingTags = true;
    } else if (state.reasoningFromThinkingTags) {
      state.reasoningText = "";
      state.reasoningFromThinkingTags = false;
    }
    updateStreamingAssistantMessage(
      ctx.ownerUserId,
      ctx.chatId,
      ctx.assistantMessageId,
      {
        content: state.assistantText,
        reasoning: state.reasoningText || undefined,
        reasoningStatus: state.reasoningText
          ? split.reasoningOpen
            ? "streaming"
            : "complete"
          : status === "streaming"
            ? "streaming"
            : status === "complete"
              ? "complete"
              : undefined,
        status,
      },
    );
  };

  const applyRunFailure = (reason?: string | null) => {
    const message = reason?.trim() ? reason : state.lastFailureMessage ?? "Astra run failed.";
    if (message !== state.lastFailureMessage) {
      state.assistantText = state.assistantText ? `${state.assistantText}\n\n${message}` : message;
      state.lastFailureMessage = message;
    }
    state.lastStatus = "failed";
    state.runLifecycle = "finished";
    setChatActiveRun(ctx.ownerUserId, ctx.chatId, undefined);
    updateStreamingAssistantMessage(ctx.ownerUserId, ctx.chatId, ctx.assistantMessageId, {
      content: state.assistantText,
      reasoning: state.reasoningText || undefined,
      reasoningStatus: undefined,
      status: "failed",
    });
  };

  switch (event.type) {
    case "session_info": {
      if (typeof event.session_id !== "string") break;
      const conflictingRun = typeof event.run_id === "string" && state.runId && state.runId !== event.run_id;
      if (event.session_id !== expectedSessionId || conflictingRun) {
        const message = conflictingRun
          ? `Runtime changed root run_id from ${state.runId} to ${event.run_id}.`
          : `Runtime returned session_id ${event.session_id}, but Web chat is bound to ${expectedSessionId}.`;
        state.protocolError = true;
        state.assistantText = message;
        state.lastStatus = "failed";
        state.runLifecycle = "finished";
        setChatActiveRun(ctx.ownerUserId, ctx.chatId, undefined);
        updateStreamingAssistantMessage(
          ctx.ownerUserId,
          ctx.chatId,
          ctx.assistantMessageId,
          {
            content: message,
            status: "failed",
          },
        );
        throw new Error(message);
      }
      if (typeof event.run_id === "string") {
        state.runId ??= event.run_id;
        setActiveRun({
          runId: event.run_id,
          status: "running",
          waitingFor: null,
        });
      }
      break;
    }

    case "artifact_publication": {
      if (!isArtifactPublicationV1(event)) throw new Error("Invalid report publication result.");
      updateStreamingAssistantMessage(ctx.ownerUserId, ctx.chatId, ctx.assistantMessageId, { artifactPublication: event });
      break;
    }

    case "explain_analyze":
    case "stream_gap": {
      applyExplainAnalyzeObservation(event, ctx);
      break;
    }

    case "run_started": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "running";
      setActiveRun({
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      });
      break;
    }

    case "run_blocked": {
      state.runLifecycle = "blocked";
      state.statusFeedbackText = explicitEventMessage(event) || undefined;
      // RunBlockedEvent carries no run_id field; keep whatever runId is already known
      const runId = state.runId;
      const waitingFor = blockedWaitingFor(event);
      const message = explicitEventMessage(event);
      if (message && !state.assistantText) {
        state.assistantText = message;
        updateStreamingAssistantMessage(
          ctx.ownerUserId,
          ctx.chatId,
          ctx.assistantMessageId,
          {
            content: state.assistantText,
            reasoning: state.reasoningText || undefined,
            reasoningStatus: state.reasoningText ? "streaming" : undefined,
            status: "streaming",
          },
        );
      }
      if (runId) {
        setActiveRun({
          runId,
          status: "blocked",
          waitingFor,
        });
      }
      break;
    }

    case "run_waiting": {
      const runId =
        typeof event.run_id === "string" && event.run_id.trim()
          ? event.run_id
          : state.runId;
      const projection = projectRunWaitingState(event);
      state.runLifecycle = projection.status;
      if (projection.status === "blocked") {
        state.statusFeedbackText = explicitEventMessage(event) || undefined;
      }
      if (runId) {
        setActiveRun({
          runId,
          status: projection.status,
          waitingFor: projection.waitingFor,
        });
      }
      break;
    }

    case "run_paused": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "paused";
      setActiveRun({
        runId: event.run_id,
        status: "paused",
        waitingFor: null,
      });
      break;
    }

    case "run_input_queued": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "running";
      setActiveRun({
        runId: event.run_id,
        status: "input-queued",
        waitingFor: "user_input",
      });
      break;
    }

    case "run_resumed": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "running";
      setActiveRun({
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      });
      break;
    }

    case "run_error": {
      state.errorCode = typeof event.error_code === "string" ? event.error_code : undefined;
      applyRunFailure(eventMessage(event, "Astra run failed."));
      break;
    }

    case "run_interrupted": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "paused";
      const waitingFor =
        typeof (event as RunInterruptedEvent).waiting_for === "string"
          ? (event as RunInterruptedEvent).waiting_for!
          : "user_resume";
      setActiveRun({
        runId: event.run_id,
        status: "paused",
        waitingFor,
      });
      break;
    }

    case "text_delta": {
      state.assistantRawText = mergeTextDelta(
        state.assistantRawText,
        event.content,
      );
      applyAssistantText(state.assistantRawText, "streaming");
      break;
    }

    case "tool_call_end": {
      const artifacts = artifactsFromToolCallEnd(
        event as unknown as Record<string, unknown>,
      );
      if (artifacts.length === 0) break;
      state.artifacts = mergeChatArtifacts(state.artifacts ?? [], artifacts);
      updateStreamingAssistantMessage(
        ctx.ownerUserId,
        ctx.chatId,
        ctx.assistantMessageId,
        { artifacts: state.artifacts },
      );
      break;
    }

    case "reasoning_delta":
    case "thinking_delta":
    case "reasoning_message_content": {
      state.reasoningText = mergeTextDelta(state.reasoningText, event.content);
      updateStreamingAssistantMessage(
        ctx.ownerUserId,
        ctx.chatId,
        ctx.assistantMessageId,
        {
          reasoning: state.reasoningText,
          reasoningStatus: "streaming",
          status: "streaming",
        },
      );
      break;
    }

    case "reasoning_done":
    case "thinking_done": {
      updateStreamingAssistantMessage(
        ctx.ownerUserId,
        ctx.chatId,
        ctx.assistantMessageId,
        {
          reasoning: state.reasoningText,
          reasoningStatus: "complete",
          status: "streaming",
        },
      );
      break;
    }

    case "text_done": {
      state.assistantRawText = event.full_text;
      applyAssistantText(state.assistantRawText, "streaming");
      break;
    }

    case "turn_complete": {
      if (typeof event.assistant_text === "string") {
        state.assistantRawText = event.assistant_text;
        applyAssistantText(
          state.assistantRawText,
          state.lastStatus === "streaming" ? "complete" : state.lastStatus,
        );
      }
      break;
    }

    case "error": {
      const message = (event as StreamErrorEvent).message || "Astra stream failed.";
      state.assistantText = state.assistantText || message;
      state.lastStatus = "failed";
      updateStreamingAssistantMessage(
        ctx.ownerUserId,
        ctx.chatId,
        ctx.assistantMessageId,
        {
          content: state.assistantText,
          status: "failed",
        },
      );
      break;
    }

    case "run_finished": {
      const status =
        typeof (event as RunFinishedEvent).status === "string"
          ? (event as RunFinishedEvent).status!
          : "completed";
      if (status === "failed") {
        applyRunFailure((event as RunFinishedEvent).error);
        break;
      }
      if (status === "paused" || status === "interrupted") {
        state.runLifecycle = "paused";
        state.lastStatus = "complete";
        if (typeof event.run_id === "string") {
          const waitingFor =
            typeof (event as RunFinishedEvent).waiting_for === "string"
              ? (event as RunFinishedEvent).waiting_for!
              : "user_resume";
          setActiveRun({
            runId: event.run_id,
            status: "paused",
            waitingFor,
          });
        }
        updateStreamingAssistantMessage(
          ctx.ownerUserId,
          ctx.chatId,
          ctx.assistantMessageId,
          {
            content: state.assistantText,
            reasoning: state.reasoningText || undefined,
            reasoningStatus: state.reasoningText ? "complete" : undefined,
            status: "complete",
          },
        );
        break;
      }
      state.runLifecycle = "finished";
      setChatActiveRun(ctx.ownerUserId, ctx.chatId, undefined);
      if (status === "cancelled") {
        state.assistantText = state.assistantText || "Stopped.";
        state.lastStatus = "complete";
      } else {
        state.lastStatus = "complete";
      }
      updateStreamingAssistantMessage(
        ctx.ownerUserId,
        ctx.chatId,
        ctx.assistantMessageId,
        {
          content: state.assistantText,
          reasoning: state.reasoningText || undefined,
          reasoningStatus:
            state.lastStatus === "complete" ? "complete" : undefined,
          status: state.lastStatus,
        },
      );
      break;
    }

    default:
      // All other event types (tool_call, plan_*, agent_*, usage, ping, etc.)
      // are not handled by the web stream event handler.
      break;
  }
}
