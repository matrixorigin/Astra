import type {
  StreamEvent,
  RunFinishedEvent,
  RunInterruptedEvent,
  StreamErrorEvent,
} from "@astra/sdk";
import {
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { mergeTextDelta, splitThinkingTags } from "@/lib/api/stream-text";
import {
  extractBlockedReason,
  projectRunWaitingState,
} from "@/lib/run-status-messages";

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
  reasoningText: string;
  statusFeedbackText?: string;
  reasoningFromThinkingTags?: boolean;
  lastStatus: "streaming" | "complete" | "failed";
  protocolError: boolean;
  runLifecycle: "running" | "paused" | "waiting" | "blocked" | "finished";
  nextEventIndex?: number;
}

function normalizeEventIndex(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return null;
  }
  return Math.trunc(value);
}

export function blockedWaitingFor(event: StreamEvent) {
  return (
    extractBlockedReason(
      event as {
        type?: string;
        reason?: string;
        error_kind?: string;
        blocked?: boolean;
      },
    ) ?? "blocked"
  );
}

export function eventMessage(event: StreamEvent, fallback: string): string {
  for (const key of ["message", "error", "user_message", "reason"] as const) {
    const value = (event as Record<string, unknown>)[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return fallback;
}

function eventStatusFeedback(event: StreamEvent): string | undefined {
  for (const key of ["message", "error", "user_message"] as const) {
    const value = (event as Record<string, unknown>)[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return undefined;
}

export function isRunBlockedEvent(type: string): boolean {
  return type === "run_blocked";
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

  switch (event.type) {
    case "session_info": {
      if (typeof event.session_id !== "string") break;
      if (event.session_id !== expectedSessionId) {
        const message = `Runtime returned session_id ${event.session_id}, but Web chat is bound to ${expectedSessionId}.`;
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
        state.runId = event.run_id;
        setActiveRun({
          runId: event.run_id,
          status: "running",
          waitingFor: null,
        });
      }
      break;
    }

    case "run_started": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "running";
      state.runId = event.run_id;
      setActiveRun({
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      });
      break;
    }

    case "run_blocked": {
      state.runLifecycle = "blocked";
      state.statusFeedbackText = eventStatusFeedback(event);
      // RunBlockedEvent carries no run_id field; keep whatever runId is already known
      const runId = state.runId;
      const waitingFor = blockedWaitingFor(event);
      if (runId) {
        state.runId = runId;
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
        state.statusFeedbackText = eventStatusFeedback(event);
      }
      if (runId) {
        state.runId = runId;
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
      state.runId = event.run_id;
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
      state.runId = event.run_id;
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
      state.runId = event.run_id;
      setActiveRun({
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      });
      break;
    }

    case "run_error": {
      const message = eventMessage(event, "Astra run failed.");
      state.assistantText = state.assistantText || message;
      state.lastStatus = "failed";
      state.runLifecycle = "finished";
      setChatActiveRun(ctx.ownerUserId, ctx.chatId, undefined);
      updateStreamingAssistantMessage(
        ctx.ownerUserId,
        ctx.chatId,
        ctx.assistantMessageId,
        {
          content: state.assistantText,
          reasoning: state.reasoningText || undefined,
          reasoningStatus: undefined,
          status: "failed",
        },
      );
      break;
    }

    case "run_interrupted": {
      if (typeof event.run_id !== "string") break;
      state.runLifecycle = "paused";
      state.runId = event.run_id;
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
      if (typeof event.run_id === "string") {
        state.runId = event.run_id;
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
      } else if (status === "failed") {
        const err = (event as RunFinishedEvent).error;
        const message = typeof err === "string" ? err : state.assistantText;
        state.assistantText = message || state.assistantText;
        state.lastStatus = "failed";
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
