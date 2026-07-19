import { requestJson, toQuery } from "@/lib/api/request";
import { WebApiError } from "@/lib/api/errors";
import { mergeTextDelta, splitThinkingTags } from "@/lib/api/stream-text";
import { projectRunWaitingState } from "@/lib/run-status-messages";
import {
  blockedWaitingFor,
  eventMessage,
  isRunBlockedEvent,
} from "@/lib/api/stream-event-helpers";
import type {
  ChatDetail,
  ChatMessage,
  ChatListResponse,
  CreateChatRequest,
  CreateChatResponse,
  QueueRunInputResponse,
  SendMessageRequest,
  SendMessageResponse,
  ActiveRunMutationResponse,
  WorkSurfaceRunResponse,
  ChatInsightsResponse,
  EdgeStatusResponse,
  RuntimeCapabilitiesResponse,
  WorkspaceSelection,
} from "@/lib/api/types";
import {
  parseWorkSurfaceEvent,
  type WorkSurfaceEvent,
  type WorkSurfaceResponse,
} from "@/lib/work-surface";

export function listChats(params: {
  projectId?: string | null;
  q?: string;
  cursor?: string | null;
  limit?: number;
  archived?: boolean;
}) {
  return requestJson<ChatListResponse>(`/api/chats${toQuery(params)}`);
}

export function createChat(payload: CreateChatRequest) {
  return requestJson<CreateChatResponse>("/api/chats", {
    method: "POST",
    body: JSON.stringify(payload),
  });
}

export function getChat(chatId: string) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`);
}

export function getChatWorkSurface(chatId: string) {
  return requestJson<WorkSurfaceResponse>(
    `/api/chats/${encodeURIComponent(chatId)}/work-surface`,
  );
}

export function getChatWorkSurfaceRun(chatId: string, runId: string) {
  return requestJson<WorkSurfaceRunResponse>(
    `/api/chats/${encodeURIComponent(
      chatId,
    )}/work-surface/runs/${encodeURIComponent(runId)}`,
  );
}

export function getChatInsights(chatId: string) {
  return requestJson<ChatInsightsResponse>(
    `/api/chats/${encodeURIComponent(chatId)}/insights`,
  );
}

export function getEdgeStatus() {
  return requestJson<EdgeStatusResponse>("/api/edges/status");
}

export function getRuntimeCapabilities() {
  return requestJson<RuntimeCapabilitiesResponse>("/api/runtime/capabilities");
}

export function archiveChat(chatId: string, archived: boolean) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: "PATCH",
    body: JSON.stringify({ archived }),
  });
}

export function updateChatModel(chatId: string, model: string) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: "PATCH",
    body: JSON.stringify({ model }),
  });
}

export function updateChatWorkspaceSelection(
  chatId: string,
  workspaceSelection: WorkspaceSelection | null,
) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: "PATCH",
    body: JSON.stringify({ workspaceSelection }),
  });
}

export function deleteChat(chatId: string) {
  return requestJson<{ deleted: true }>(
    `/api/chats/${encodeURIComponent(chatId)}`,
    {
      method: "DELETE",
    },
  );
}

export function clearArchivedChats() {
  return requestJson<{ deleted: number }>("/api/chats?archived=true", {
    method: "DELETE",
  });
}

export function sendChatMessage(chatId: string, payload: SendMessageRequest) {
  return requestJson<SendMessageResponse>(
    `/api/chats/${encodeURIComponent(chatId)}/messages`,
    {
      method: "POST",
      body: JSON.stringify(payload),
    },
  );
}

export function queueChatRunInput(chatId: string, payload: SendMessageRequest) {
  return requestJson<QueueRunInputResponse>(
    `/api/chats/${encodeURIComponent(chatId)}/input`,
    {
      method: "POST",
      body: JSON.stringify(payload),
    },
  );
}

export function stopChatRun(chatId: string) {
  return requestJson<ActiveRunMutationResponse>(
    `/api/chats/${encodeURIComponent(chatId)}/stop`,
    {
      method: "POST",
    },
  );
}

export function resumeChatRun(chatId: string) {
  return requestJson<ActiveRunMutationResponse>(
    `/api/chats/${encodeURIComponent(chatId)}/resume`,
    {
      method: "POST",
    },
  );
}

export type ChatStreamHandlers = {
  signal?: AbortSignal;
  onLocalMessages?: (messages: {
    userMessage: ChatMessage;
    assistantMessage: ChatMessage;
  }) => void;
  onArtifacts?: (artifacts: NonNullable<ChatMessage["artifacts"]>) => void;
  onReasoning?: (reasoning: string) => void;
  onReasoningDone?: (reasoning: string) => void;
  onText?: (text: string) => void;
  onSessionBound?: (session: { chatId?: string; sessionId: string }) => void;
  onRunStarted?: (runId: string) => void;
  onRunUpdated?: (run: {
    runId: string;
    status: string;
    waitingFor?: string | null;
    nextEventIndex?: number | null;
  }) => void;
  onRunFinished?: (run: {
    runId?: string;
    status: string;
    error?: string | null;
  }) => void;
  onWorkSurfaceEvent?: (event: WorkSurfaceEvent) => void;
  onCancelled?: (text: string) => void;
  onPaused?: (text: string) => void;
  onDone?: (text: string) => void;
};

type ChatStreamState = {
  runId?: string;
  rawText: string;
  text: string;
  reasoning: string;
  reasoningFromThinkingTags?: boolean;
  cancelled?: boolean;
  paused?: boolean;
  error?: string;
  errorCode?: string;
  errorStatus?: number;
  nextEventIndex?: number;
};

function normalizeEventIndex(value: unknown): number | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return null;
  }
  return Math.trunc(value);
}

function runUpdate(
  state: ChatStreamState,
  run: {
    runId: string;
    status: string;
    waitingFor?: string | null;
  },
) {
  return {
    ...run,
    nextEventIndex: state.nextEventIndex ?? null,
  };
}

export { splitThinkingTags };

function parseSseFrame(frame: string) {
  const data = frame
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .join("\n");

  if (!data || data === "[DONE]") {
    return null;
  }

  try {
    return JSON.parse(data) as Record<string, unknown>;
  } catch {
    return null;
  }
}

const WORK_SURFACE_STREAM_EVENT_TYPES = new Set([
  "task_board_snapshot",
  "workspace_bound",
  "executor_bound",
  "executor_status_changed",
  "tool_call",
  "tool_call_start",
  "tool_routing_decision",
  "tool_transport_started",
  "tool_transport_completed",
  "tool_transport_failed",
  "tool_call_end",
  "agent_delegated",
  "agent_spawned",
  "agent_live_event",
  "agent_live_gap",
  "stream_gap",
  "agent_progress",
  "agent_completed",
  "agent_failed",
  "agent_waiting",
  "agent_cancelled",
  "agent_interrupted",
  "run_waiting",
  "run_blocked",
]);

function isWorkSurfaceStreamEvent(type: string) {
  return WORK_SURFACE_STREAM_EVENT_TYPES.has(type) || isRunBlockedEvent(type);
}

function forwardWorkSurfaceEvent(
  event: Record<string, unknown>,
  handlers: ChatStreamHandlers,
) {
  const parsed = parseWorkSurfaceEvent(event);
  if (parsed) handlers.onWorkSurfaceEvent?.(parsed);
}

function applyAssistantText(
  rawText: string,
  state: ChatStreamState,
  handlers: ChatStreamHandlers,
) {
  const split = splitThinkingTags(rawText);
  state.text = split.visibleText;
  if (split.hasThinking) {
    state.reasoning = split.reasoning;
    state.reasoningFromThinkingTags = true;
    if (split.reasoningOpen) {
      handlers.onReasoning?.(state.reasoning);
    } else {
      handlers.onReasoningDone?.(state.reasoning);
    }
  } else if (state.reasoningFromThinkingTags) {
    state.reasoning = "";
    state.reasoningFromThinkingTags = false;
  }
  handlers.onText?.(state.text);
}

function applyStreamEvent(
  event: Record<string, unknown>,
  state: ChatStreamState,
  handlers: ChatStreamHandlers,
) {
  const type = typeof event.type === "string" ? event.type : "";
  const eventIndex = normalizeEventIndex(event.index);
  if (eventIndex !== null) {
    state.nextEventIndex = Math.max(state.nextEventIndex ?? 0, eventIndex + 1);
  }

  if (
    type === "local_messages" &&
    event.user_message &&
    event.assistant_message
  ) {
    handlers.onLocalMessages?.({
      userMessage: event.user_message as ChatMessage,
      assistantMessage: event.assistant_message as ChatMessage,
    });
    return;
  }

  if (type === "session_info" && typeof event.run_id === "string") {
    state.runId = event.run_id;
    if (typeof event.session_id === "string") {
      handlers.onSessionBound?.({ sessionId: event.session_id });
    }
    handlers.onRunStarted?.(event.run_id);
    handlers.onRunUpdated?.(
      runUpdate(state, {
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      }),
    );
    return;
  }

  if (type === "session_bound" && typeof event.session_id === "string") {
    handlers.onSessionBound?.({
      chatId: typeof event.chat_id === "string" ? event.chat_id : undefined,
      sessionId: event.session_id,
    });
    return;
  }

  if (type === "text_delta" && typeof event.content === "string") {
    state.rawText = mergeTextDelta(state.rawText, event.content);
    applyAssistantText(state.rawText, state, handlers);
    return;
  }

  if (type === "run_started" && typeof event.run_id === "string") {
    state.runId = event.run_id;
    forwardWorkSurfaceEvent(event, handlers);
    handlers.onRunStarted?.(event.run_id);
    handlers.onRunUpdated?.(
      runUpdate(state, {
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      }),
    );
    return;
  }

  if (type === "run_input_queued" && typeof event.run_id === "string") {
    state.runId = event.run_id;
    forwardWorkSurfaceEvent(event, handlers);
    handlers.onRunUpdated?.(
      runUpdate(state, {
        runId: event.run_id,
        status: "input-queued",
        waitingFor: "user_input",
      }),
    );
    return;
  }

  if (isRunBlockedEvent(type)) {
    forwardWorkSurfaceEvent(event, handlers);
    const runId =
      typeof event.run_id === "string" && event.run_id.trim()
        ? event.run_id
        : state.runId;
    state.paused = true;
    if (runId) {
      state.runId = runId;
      handlers.onRunUpdated?.(
        runUpdate(state, {
          runId,
          status: "blocked",
          waitingFor: blockedWaitingFor(event),
        }),
      );
    }
    return;
  }

  if (type === "run_waiting") {
    forwardWorkSurfaceEvent(event, handlers);
    const runId =
      typeof event.run_id === "string" && event.run_id.trim()
        ? event.run_id
        : state.runId;
    const projection = projectRunWaitingState(
      event as { waiting_for?: string; reason?: string; error_kind?: string },
    );
    state.paused = true;
    if (runId) {
      state.runId = runId;
      handlers.onRunUpdated?.(
        runUpdate(state, {
          runId,
          status: projection.status,
          waitingFor: projection.waitingFor,
        }),
      );
    }
    return;
  }

  if (isWorkSurfaceStreamEvent(type)) {
    forwardWorkSurfaceEvent(event, handlers);
    return;
  }

  if (type === "run_paused" && typeof event.run_id === "string") {
    state.runId = event.run_id;
    forwardWorkSurfaceEvent(event, handlers);
    state.paused = true;
    handlers.onRunUpdated?.(
      runUpdate(state, {
        runId: event.run_id,
        status: "paused",
        waitingFor: null,
      }),
    );
    return;
  }

  if (type === "run_resumed" && typeof event.run_id === "string") {
    state.runId = event.run_id;
    forwardWorkSurfaceEvent(event, handlers);
    state.paused = false;
    handlers.onRunUpdated?.(
      runUpdate(state, {
        runId: event.run_id,
        status: "running",
        waitingFor: null,
      }),
    );
    return;
  }

  if (type === "run_error") {
    forwardWorkSurfaceEvent(event, handlers);
    const message = eventMessage(event, "Astra run failed.");
    state.error = message;
    if (typeof event.run_id === "string") {
      state.runId = event.run_id;
      handlers.onRunUpdated?.(
        runUpdate(state, {
          runId: event.run_id,
          status: "failed",
          waitingFor: null,
        }),
      );
    }
    return;
  }

  if (type === "run_interrupted") {
    forwardWorkSurfaceEvent(event, handlers);
    state.paused = true;
    if (typeof event.run_id === "string") {
      state.runId = event.run_id;
      handlers.onRunUpdated?.(
        runUpdate(state, {
          runId: event.run_id,
          status: "paused",
          waitingFor:
            typeof event.waiting_for === "string"
              ? event.waiting_for
              : "user_resume",
        }),
      );
    }
    return;
  }

  if (type === "artifacts" && Array.isArray(event.artifacts)) {
    handlers.onArtifacts?.(
      event.artifacts as NonNullable<ChatMessage["artifacts"]>,
    );
    return;
  }

  if (
    (type === "reasoning_delta" ||
      type === "thinking_delta" ||
      type === "reasoning_message_content") &&
    typeof event.content === "string"
  ) {
    state.reasoning = mergeTextDelta(state.reasoning, event.content);
    handlers.onReasoning?.(state.reasoning);
    return;
  }

  if (type === "reasoning_done" || type === "thinking_done") {
    handlers.onReasoningDone?.(state.reasoning);
    return;
  }

  if (type === "text_done" && typeof event.full_text === "string") {
    state.rawText = event.full_text;
    applyAssistantText(state.rawText, state, handlers);
    return;
  }

  if (type === "turn_complete" && typeof event.assistant_text === "string") {
    state.rawText = event.assistant_text;
    applyAssistantText(state.rawText, state, handlers);
    return;
  }

  if (type === "error") {
    state.error =
      typeof event.message === "string"
        ? event.message
        : "Astra stream failed.";
    state.errorCode = typeof event.code === "string" ? event.code : undefined;
    state.errorStatus =
      typeof event.status === "number" &&
      Number.isFinite(event.status) &&
      event.status >= 400
        ? Math.trunc(event.status)
        : undefined;
    return;
  }

  if (type === "run_finished") {
    forwardWorkSurfaceEvent(event, handlers);
    const status =
      typeof event.status === "string" ? event.status : "completed";
    if (typeof event.run_id === "string") {
      state.runId = event.run_id;
    }
    if (status === "paused" || status === "interrupted") {
      state.paused = true;
      if (typeof event.run_id === "string") {
        handlers.onRunUpdated?.(
          runUpdate(state, {
            runId: event.run_id,
            status: "paused",
            waitingFor:
              typeof event.waiting_for === "string"
                ? event.waiting_for
                : "user_resume",
          }),
        );
      }
      return;
    }
    handlers.onRunFinished?.({
      runId: typeof event.run_id === "string" ? event.run_id : undefined,
      status,
      error: typeof event.error === "string" ? event.error : null,
    });
    if (status === "cancelled") {
      state.cancelled = true;
      return;
    }
    if (status === "failed") {
      state.error =
        typeof event.error === "string"
          ? event.error
          : (state.error ?? "Astra run failed.");
    }
    state.paused = false;
  }
}

async function consumeChatStream(
  response: Response,
  handlers: ChatStreamHandlers,
) {
  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    let code: string | undefined;
    try {
      const body = (await response.json()) as {
        error?: string;
        detail?: string;
        code?: string;
      };
      detail = body.error ?? body.detail ?? detail;
      code = typeof body.code === "string" ? body.code : undefined;
    } catch {
      // Preserve the HTTP status.
    }
    throw new WebApiError(response.status, detail, code);
  }

  if (!response.body) {
    throw new Error("Astra stream response had no body.");
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const state: ChatStreamState = { rawText: "", text: "", reasoning: "" };
  let buffer = "";
  let abortListener: (() => void) | undefined;

  try {
    if (handlers.signal) {
      abortListener = () => {
        void reader.cancel();
      };
      handlers.signal.addEventListener("abort", abortListener, { once: true });
    }
    for (;;) {
      if (handlers.signal?.aborted) {
        await reader.cancel();
        throw new DOMException("The chat stream was aborted.", "AbortError");
      }
      const { value, done } = await reader.read();
      if (handlers.signal?.aborted) {
        throw new DOMException("The chat stream was aborted.", "AbortError");
      }
      if (done) {
        break;
      }
      buffer += decoder.decode(value, { stream: true });
      const frames = buffer.split(/\r?\n\r?\n/);
      buffer = frames.pop() ?? "";
      for (const frame of frames) {
        const event = parseSseFrame(frame);
        if (event) {
          applyStreamEvent(event, state, handlers);
        }
      }
    }

    const tail = decoder.decode();
    if (tail) {
      buffer += tail;
    }
    if (buffer.trim()) {
      const event = parseSseFrame(buffer);
      if (event) {
        applyStreamEvent(event, state, handlers);
      }
    }
  } finally {
    if (handlers.signal && abortListener) {
      handlers.signal.removeEventListener("abort", abortListener);
    }
    reader.releaseLock();
  }

  if (state.error) {
    throw new WebApiError(state.errorStatus ?? 500, state.error, state.errorCode);
  }
  if (state.cancelled) {
    handlers.onCancelled?.(state.text);
    return state.text;
  }
  if (state.paused) {
    handlers.onPaused?.(state.text);
    return state.text;
  }
  handlers.onDone?.(state.text);
  return state.text;
}

export async function streamChatMessage(
  chatId: string,
  payload: SendMessageRequest,
  handlers: ChatStreamHandlers,
) {
  const response = await fetch(
    `/api/chats/${encodeURIComponent(chatId)}/stream`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      signal: handlers.signal,
    },
  );
  return consumeChatStream(response, handlers);
}

export async function streamExistingChatRun(
  chatId: string,
  runId: string,
  handlers: ChatStreamHandlers,
  options?: {
    nextEventIndex?: number | null;
    assistantMessageId?: string | null;
  },
) {
  const params = new URLSearchParams({ runId });
  const nextEventIndex = normalizeEventIndex(options?.nextEventIndex);
  if (nextEventIndex !== null) {
    params.set("last_index", String(nextEventIndex));
  }
  if (options?.assistantMessageId?.trim()) {
    params.set("assistantMessageId", options.assistantMessageId.trim());
  }
  const response = await fetch(
    `/api/chats/${encodeURIComponent(chatId)}/stream?${params.toString()}`,
    { method: "GET", signal: handlers.signal },
  );
  return consumeChatStream(response, handlers);
}

export function updateChatProject(chatId: string, projectId: string | null) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: "PATCH",
    body: JSON.stringify({ projectId }),
  });
}
