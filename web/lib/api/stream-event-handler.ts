import type { StreamEvent } from "@astra/sdk";
import {
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { mergeTextDelta, splitThinkingTags } from "@/lib/api/stream-text";
import {
  runWaitingStatusMessage,
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

export function explicitEventMessage(event: StreamEvent): string {
  for (const key of ["message", "error", "user_message"] as const) {
    const value = (event as Record<string, unknown>)[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return "";
}

export function isRunBlockedEvent(type: string): boolean {
  return type === "run_blocked";
}

export function applyStreamEvent(
  event: StreamEvent,
  ctx: StreamEventContext,
  state: StreamEventState,
): void {
  const type = typeof event.type === "string" ? event.type : "";
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

  if (type === "session_info" && typeof event.session_id === "string") {
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
    return;
  }

  if (type === "run_started" && typeof event.run_id === "string") {
    state.runLifecycle = "running";
    state.runId = event.run_id;
    setActiveRun({
      runId: event.run_id,
      status: "running",
      waitingFor: null,
    });
    return;
  }

  if (isRunBlockedEvent(type)) {
    state.runLifecycle = "blocked";
    const runId =
      typeof event.run_id === "string" && event.run_id.trim()
        ? event.run_id
        : state.runId;
    const waitingFor = blockedWaitingFor(event);
    const message =
      explicitEventMessage(event) || runWaitingStatusMessage(waitingFor, true);
    if (
      !state.assistantRawText.trim() &&
      !state.assistantText.trim() &&
      message
    ) {
      applyAssistantText(message, "streaming");
    }
    if (runId) {
      state.runId = runId;
      setActiveRun({
        runId,
        status: "blocked",
        waitingFor,
      });
    }
    return;
  }

  if (type === "run_waiting") {
    const runId =
      typeof event.run_id === "string" && event.run_id.trim()
        ? event.run_id
        : state.runId;
    const projection = projectRunWaitingState(
      event as { waiting_for?: string; reason?: string; error_kind?: string },
    );
    state.runLifecycle = projection.status;
    const message =
      explicitEventMessage(event) ||
      runWaitingStatusMessage(projection.waitingFor, projection.blocked);
    if (
      !state.assistantRawText.trim() &&
      !state.assistantText.trim() &&
      message
    ) {
      applyAssistantText(message, "streaming");
    }
    if (runId) {
      state.runId = runId;
      setActiveRun({
        runId,
        status: projection.status,
        waitingFor: projection.waitingFor,
      });
    }
    return;
  }

  if (type === "run_paused" && typeof event.run_id === "string") {
    state.runLifecycle = "paused";
    state.runId = event.run_id;
    setActiveRun({
      runId: event.run_id,
      status: "paused",
      waitingFor: null,
    });
    return;
  }

  if (type === "run_input_queued" && typeof event.run_id === "string") {
    state.runLifecycle = "running";
    state.runId = event.run_id;
    setActiveRun({
      runId: event.run_id,
      status: "input-queued",
      waitingFor: "user_input",
    });
    return;
  }

  if (type === "run_resumed" && typeof event.run_id === "string") {
    state.runLifecycle = "running";
    state.runId = event.run_id;
    setActiveRun({
      runId: event.run_id,
      status: "running",
      waitingFor: null,
    });
    return;
  }

  if (type === "run_error") {
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
    return;
  }

  if (type === "run_interrupted" && typeof event.run_id === "string") {
    const message = eventMessage(event, "");
    state.runLifecycle = "paused";
    state.runId = event.run_id;
    if (
      !state.assistantRawText.trim() &&
      !state.assistantText.trim() &&
      message
    ) {
      applyAssistantText(message, "streaming");
    }
    setActiveRun({
      runId: event.run_id,
      status: "paused",
      waitingFor:
        typeof event.waiting_for === "string"
          ? event.waiting_for
          : "user_resume",
    });
    return;
  }

  if (type === "text_delta" && typeof event.content === "string") {
    state.assistantRawText = mergeTextDelta(
      state.assistantRawText,
      event.content,
    );
    applyAssistantText(state.assistantRawText, "streaming");
    return;
  }

  if (
    (type === "reasoning_delta" ||
      type === "thinking_delta" ||
      type === "reasoning_message_content") &&
    typeof event.content === "string"
  ) {
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
    return;
  }

  if (type === "reasoning_done" || type === "thinking_done") {
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
    return;
  }

  if (type === "text_done" && typeof event.full_text === "string") {
    state.assistantRawText = event.full_text;
    applyAssistantText(state.assistantRawText, "streaming");
    return;
  }

  if (type === "turn_complete" && typeof event.assistant_text === "string") {
    state.assistantRawText = event.assistant_text;
    applyAssistantText(
      state.assistantRawText,
      state.lastStatus === "streaming" ? "complete" : state.lastStatus,
    );
    return;
  }

  if (type === "error") {
    const message =
      typeof event.message === "string"
        ? event.message
        : "Astra stream failed.";
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
    return;
  }

  if (type === "run_finished") {
    const status =
      typeof event.status === "string" ? event.status : "completed";
    if (typeof event.run_id === "string") {
      state.runId = event.run_id;
    }
    if (status === "paused" || status === "interrupted") {
      state.runLifecycle = "paused";
      if (typeof event.run_id === "string") {
        setActiveRun({
          runId: event.run_id,
          status: "paused",
          waitingFor:
            typeof event.waiting_for === "string"
              ? event.waiting_for
              : "user_resume",
        });
      }
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
      return;
    }
    state.runLifecycle = "finished";
    setChatActiveRun(ctx.ownerUserId, ctx.chatId, undefined);
    if (status === "cancelled") {
      state.assistantText = state.assistantText || "Stopped.";
      state.lastStatus = "complete";
    } else if (status === "failed") {
      const message =
        typeof event.error === "string" ? event.error : state.assistantText;
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
  }
}
