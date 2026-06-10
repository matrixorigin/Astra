import {
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { mergeTextDelta, splitThinkingTags } from "@/lib/api/stream-text";

export interface StreamEventContext {
  ownerUserId: string;
  chatId: string;
  assistantMessageId: string;
  getSessionId: () => string;
}

export interface StreamEventState {
  assistantText: string;
  assistantRawText: string;
  reasoningText: string;
  reasoningFromThinkingTags?: boolean;
  lastStatus: "streaming" | "complete" | "failed";
  protocolError: boolean;
  runLifecycle: "running" | "paused" | "finished";
}

export function applyStreamEvent(
  event: Record<string, unknown>,
  ctx: StreamEventContext,
  state: StreamEventState,
): void {
  const type = typeof event.type === "string" ? event.type : "";
  const expectedSessionId = ctx.getSessionId();

  if (state.protocolError) {
    return;
  }

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
      setChatActiveRun(ctx.ownerUserId, ctx.chatId, {
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      });
    }
    return;
  }

  if (type === "run_started" && typeof event.run_id === "string") {
    state.runLifecycle = "running";
    setChatActiveRun(ctx.ownerUserId, ctx.chatId, {
      runId: event.run_id,
      status: "running",
      waitingFor: null,
    });
    return;
  }

  if (type === "run_paused" && typeof event.run_id === "string") {
    state.runLifecycle = "paused";
    setChatActiveRun(ctx.ownerUserId, ctx.chatId, {
      runId: event.run_id,
      status: "paused",
      waitingFor: null,
    });
    return;
  }

  if (type === "run_resumed" && typeof event.run_id === "string") {
    state.runLifecycle = "running";
    setChatActiveRun(ctx.ownerUserId, ctx.chatId, {
      runId: event.run_id,
      status: "running",
      waitingFor: null,
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
