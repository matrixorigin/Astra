import { requestJson, toQuery } from '@/lib/api/request';
import { WebApiError } from '@/lib/api/errors';
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
} from '@/lib/api/types';

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
  return requestJson<CreateChatResponse>('/api/chats', {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}

export function getChat(chatId: string) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`);
}

export function archiveChat(chatId: string, archived: boolean) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: 'PATCH',
    body: JSON.stringify({ archived }),
  });
}

export function updateChatModel(chatId: string, model: string) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: 'PATCH',
    body: JSON.stringify({ model }),
  });
}

export function deleteChat(chatId: string) {
  return requestJson<{ deleted: true }>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: 'DELETE',
  });
}

export function clearArchivedChats() {
  return requestJson<{ deleted: number }>('/api/chats?archived=true', {
    method: 'DELETE',
  });
}

export function sendChatMessage(chatId: string, payload: SendMessageRequest) {
  return requestJson<SendMessageResponse>(`/api/chats/${encodeURIComponent(chatId)}/messages`, {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}

export function queueChatRunInput(chatId: string, payload: SendMessageRequest) {
  return requestJson<QueueRunInputResponse>(`/api/chats/${encodeURIComponent(chatId)}/input`, {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}

export function stopChatRun(chatId: string) {
  return requestJson<ActiveRunMutationResponse>(`/api/chats/${encodeURIComponent(chatId)}/stop`, {
    method: 'POST',
  });
}

export function resumeChatRun(chatId: string) {
  return requestJson<ActiveRunMutationResponse>(`/api/chats/${encodeURIComponent(chatId)}/resume`, {
    method: 'POST',
  });
}

export type ChatStreamHandlers = {
  onLocalMessages?: (messages: { userMessage: ChatMessage; assistantMessage: ChatMessage }) => void;
  onArtifacts?: (artifacts: NonNullable<ChatMessage['artifacts']>) => void;
  onReasoning?: (reasoning: string) => void;
  onReasoningDone?: (reasoning: string) => void;
  onText?: (text: string) => void;
  onRunStarted?: (runId: string) => void;
  onRunUpdated?: (run: { runId: string; status: string; waitingFor?: string | null }) => void;
  onRunFinished?: (run: { runId?: string; status: string; error?: string | null }) => void;
  onCancelled?: (text: string) => void;
  onPaused?: (text: string) => void;
  onDone?: (text: string) => void;
};

type ChatStreamState = {
  rawText: string;
  text: string;
  reasoning: string;
  cancelled?: boolean;
  paused?: boolean;
  error?: string;
};

type ThinkingSplit = {
  visibleText: string;
  reasoning: string;
  hasThinking: boolean;
  reasoningOpen: boolean;
};

const THINKING_TAGS = [
  ['<thinking>', '</thinking>'],
  ['<think>', '</think>'],
] as const;

function parseSseFrame(frame: string) {
  const data = frame
    .split(/\r?\n/)
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trim())
    .join('\n');

  if (!data || data === '[DONE]') {
    return null;
  }

  try {
    return JSON.parse(data) as Record<string, unknown>;
  } catch {
    return null;
  }
}

function mergeTextDelta(current: string, delta: string) {
  if (!delta || current === delta || current.endsWith(delta)) {
    return current;
  }
  if (delta.startsWith(current)) {
    return delta;
  }
  return `${current}${delta}`;
}

export function splitThinkingTags(text: string): ThinkingSplit {
  const lower = text.toLowerCase();
  let cursor = 0;
  let visibleText = '';
  let reasoning = '';
  let hasThinking = false;
  let reasoningOpen = false;

  for (;;) {
    let match: { openIndex: number; openTag: string; closeTag: string } | null = null;
    for (const [openTag, closeTag] of THINKING_TAGS) {
      const openIndex = lower.indexOf(openTag, cursor);
      if (openIndex !== -1 && (!match || openIndex < match.openIndex)) {
        match = { openIndex, openTag, closeTag };
      }
    }

    if (!match) {
      let orphanClose: { closeIndex: number; closeTag: string } | null = null;
      for (const [, closeTag] of THINKING_TAGS) {
        const closeIndex = lower.indexOf(closeTag, cursor);
        if (closeIndex !== -1 && (!orphanClose || closeIndex < orphanClose.closeIndex)) {
          orphanClose = { closeIndex, closeTag };
        }
      }

      if (orphanClose) {
        hasThinking = true;
        reasoning += text.slice(cursor, orphanClose.closeIndex);
        cursor = orphanClose.closeIndex + orphanClose.closeTag.length;
        continue;
      }

      visibleText += text.slice(cursor);
      break;
    }

    hasThinking = true;
    visibleText += text.slice(cursor, match.openIndex);
    const reasoningStart = match.openIndex + match.openTag.length;
    const closeIndex = lower.indexOf(match.closeTag, reasoningStart);

    if (closeIndex === -1) {
      reasoning += text.slice(reasoningStart);
      reasoningOpen = true;
      break;
    }

    reasoning += text.slice(reasoningStart, closeIndex);
    cursor = closeIndex + match.closeTag.length;
  }

  return {
    visibleText: visibleText.replace(/\n{3,}/g, '\n\n').trim(),
    reasoning: reasoning.replace(/\n{3,}/g, '\n\n').trim(),
    hasThinking,
    reasoningOpen,
  };
}

function applyAssistantText(rawText: string, state: ChatStreamState, handlers: ChatStreamHandlers) {
  const split = splitThinkingTags(rawText);
  state.text = split.visibleText;
  if (split.hasThinking) {
    state.reasoning = split.reasoning;
    if (split.reasoningOpen) {
      handlers.onReasoning?.(state.reasoning);
    } else {
      handlers.onReasoningDone?.(state.reasoning);
    }
  }
  handlers.onText?.(state.text);
}

function applyStreamEvent(event: Record<string, unknown>, state: ChatStreamState, handlers: ChatStreamHandlers) {
  const type = typeof event.type === 'string' ? event.type : '';

  if (
    type === 'local_messages' &&
    event.user_message &&
    event.assistant_message
  ) {
    handlers.onLocalMessages?.({
      userMessage: event.user_message as ChatMessage,
      assistantMessage: event.assistant_message as ChatMessage,
    });
    return;
  }

  if (type === 'session_info' && typeof event.run_id === 'string') {
    handlers.onRunStarted?.(event.run_id);
    handlers.onRunUpdated?.({ runId: event.run_id, status: 'running', waitingFor: null });
    return;
  }

  if (type === 'text_delta' && typeof event.content === 'string') {
    state.rawText = mergeTextDelta(state.rawText, event.content);
    applyAssistantText(state.rawText, state, handlers);
    return;
  }

  if (type === 'run_started' && typeof event.run_id === 'string') {
    handlers.onRunStarted?.(event.run_id);
    handlers.onRunUpdated?.({ runId: event.run_id, status: 'running', waitingFor: null });
    return;
  }

  if (type === 'run_paused' && typeof event.run_id === 'string') {
    state.paused = true;
    handlers.onRunUpdated?.({ runId: event.run_id, status: 'paused', waitingFor: null });
    return;
  }

  if (type === 'run_resumed' && typeof event.run_id === 'string') {
    state.paused = false;
    handlers.onRunUpdated?.({ runId: event.run_id, status: 'running', waitingFor: null });
    return;
  }

  if (type === 'artifacts' && Array.isArray(event.artifacts)) {
    handlers.onArtifacts?.(event.artifacts as NonNullable<ChatMessage['artifacts']>);
    return;
  }

  if (
    (type === 'reasoning_delta' || type === 'thinking_delta' || type === 'reasoning_message_content') &&
    typeof event.content === 'string'
  ) {
    state.reasoning = mergeTextDelta(state.reasoning, event.content);
    handlers.onReasoning?.(state.reasoning);
    return;
  }

  if (type === 'reasoning_done' || type === 'thinking_done') {
    handlers.onReasoningDone?.(state.reasoning);
    return;
  }

  if (type === 'text_done' && typeof event.full_text === 'string') {
    state.rawText = event.full_text;
    applyAssistantText(state.rawText, state, handlers);
    return;
  }

  if (type === 'turn_complete' && typeof event.assistant_text === 'string') {
    state.rawText = event.assistant_text;
    applyAssistantText(state.rawText, state, handlers);
    return;
  }

  if (type === 'error') {
    state.error = typeof event.message === 'string' ? event.message : 'Astra stream failed.';
    return;
  }

  if (type === 'run_finished') {
    const status = typeof event.status === 'string' ? event.status : 'completed';
    handlers.onRunFinished?.({
      runId: typeof event.run_id === 'string' ? event.run_id : undefined,
      status,
      error: typeof event.error === 'string' ? event.error : null,
    });
    if (status === 'cancelled') {
      state.cancelled = true;
      return;
    }
    if (status === 'failed') {
      state.error = typeof event.error === 'string' ? event.error : state.error ?? 'Astra run failed.';
    }
    state.paused = false;
  }
}

async function consumeChatStream(response: Response, handlers: ChatStreamHandlers) {
  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const body = (await response.json()) as { error?: string; detail?: string };
      detail = body.error ?? body.detail ?? detail;
    } catch {
      // Preserve the HTTP status.
    }
    throw new WebApiError(response.status, detail);
  }

  if (!response.body) {
    throw new Error('Astra stream response had no body.');
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const state: ChatStreamState = { rawText: '', text: '', reasoning: '' };
  let buffer = '';

  for (;;) {
    const { value, done } = await reader.read();
    if (done) {
      break;
    }
    buffer += decoder.decode(value, { stream: true });
    const frames = buffer.split(/\r?\n\r?\n/);
    buffer = frames.pop() ?? '';
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

  if (state.error) {
    throw new Error(state.error);
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
  const response = await fetch(`/api/chats/${encodeURIComponent(chatId)}/stream`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  return consumeChatStream(response, handlers);
}

export async function streamExistingChatRun(
  chatId: string,
  runId: string,
  handlers: ChatStreamHandlers,
) {
  const response = await fetch(
    `/api/chats/${encodeURIComponent(chatId)}/stream?runId=${encodeURIComponent(runId)}`,
    { method: 'GET' },
  );
  return consumeChatStream(response, handlers);
}

export function updateChatProject(chatId: string, projectId: string | null) {
  return requestJson<ChatDetail>(`/api/chats/${encodeURIComponent(chatId)}`, {
    method: 'PATCH',
    body: JSON.stringify({ projectId }),
  });
}
