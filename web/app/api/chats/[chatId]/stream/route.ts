import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeAuth } from '@/lib/api/auth-guard';
import {
  beginStreamingMessage,
  getChat,
  resolveBackendModelName,
  updateStreamingAssistantMessage,
} from '@/lib/api/web-store';
import { getRuntimeConfig } from '@/lib/runtime-config';
import type { SendMessageRequest } from '@/lib/api/types';

const encoder = new TextEncoder();

function sseFrame(event: unknown) {
  return encoder.encode(`data: ${JSON.stringify(event)}\n\n`);
}

function eventFromSseFrame(frame: string) {
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

function messageFromResponseStatus(response: Response) {
  return `${response.status} ${response.statusText}`;
}

function normalizedActiveSkills(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }
  return [...new Set(skills.map((skill) => skill.trim()).filter(Boolean))].sort((left, right) => (
    left.localeCompare(right)
  ));
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

const THINKING_TAGS = [
  ['<thinking>', '</thinking>'],
  ['<think>', '</think>'],
] as const;

function splitThinkingTags(text: string) {
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

async function readErrorDetail(response: Response) {
  let detail = messageFromResponseStatus(response);
  try {
    const body = (await response.json()) as { detail?: string; error?: string };
    detail = body.detail ?? body.error ?? detail;
  } catch {
    // Preserve HTTP status text when the server returned a non-JSON body.
  }
  return detail;
}

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const { chatId } = await context.params;
  const body = (await request.json()) as SendMessageRequest;
  if (!body.content?.trim()) {
    return NextResponse.json({ error: 'content is required' }, { status: 400 });
  }

  const chat = getChat(chatId);
  if (!chat) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  if (chat.chat.archivedAt) {
    return NextResponse.json({ error: 'archived chat is read-only' }, { status: 409 });
  }

  const authError = await requireRuntimeAuth();
  if (authError) {
    return authError;
  }

  const started = beginStreamingMessage(chatId, body);
  if (!started) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }

  const config = await getRuntimeConfig();
  if (config.mode !== 'live' || !config.apiUrl || !config.accessToken) {
    updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
      content: 'Runtime authentication is required.',
      status: 'failed',
    });
    return NextResponse.json({ error: 'AUTH_REQUIRED' }, { status: 401 });
  }

  const model = await resolveBackendModelName(config, body.options?.model);
  const activeSkills = normalizedActiveSkills(body.options?.activeSkills);
  const backendResponse = await fetch(new URL('/chat/stream', config.apiUrl).toString(), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${config.accessToken}`,
    },
    body: JSON.stringify({
      message: body.content,
      session_id: started.backendSessionId,
      model,
      allow_skills: activeSkills.length ? activeSkills : undefined,
      context: {
        source: 'web_v1',
        transport: 'next_sse_proxy',
        edge_profile: activeSkills.length ? { active_skills: activeSkills } : undefined,
        thinking: body.options?.thinking
          ? { mode: 'adaptive', effort: 'high' }
          : { mode: 'off' },
      },
    }),
    cache: 'no-store',
  });

  if (!backendResponse.ok || !backendResponse.body) {
    const detail = await readErrorDetail(backendResponse);
    updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
      content: detail,
      status: 'failed',
    });
    return NextResponse.json({ error: detail }, { status: backendResponse.status || 502 });
  }

  let assistantText = '';
  let assistantRawText = '';
  let reasoningText = '';
  let lastStatus: 'streaming' | 'complete' | 'failed' = 'streaming';

  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      const reader = backendResponse.body?.getReader();
      if (!reader) {
        controller.enqueue(sseFrame({ type: 'error', message: 'Astra stream body is unavailable.' }));
        controller.close();
        return;
      }

      controller.enqueue(sseFrame({
        type: 'local_messages',
        user_message: started.userMessage,
        assistant_message: started.assistantMessage,
      }));

      const decoder = new TextDecoder();
      let buffer = '';

      const applyEvent = (event: Record<string, unknown>) => {
        const type = typeof event.type === 'string' ? event.type : '';

        const applyAssistantText = (rawText: string, status: 'streaming' | 'complete' | 'failed') => {
          const split = splitThinkingTags(rawText);
          assistantText = split.visibleText;
          if (split.hasThinking) {
            reasoningText = split.reasoning;
          }
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            content: assistantText,
            reasoning: reasoningText || undefined,
            reasoningStatus: reasoningText
              ? (split.reasoningOpen ? 'streaming' : 'complete')
              : (status === 'streaming' ? 'streaming' : (status === 'complete' ? 'complete' : undefined)),
            status,
          });
        };

        if (type === 'session_info' && typeof event.session_id === 'string') {
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            backendSessionId: event.session_id,
          });
          return;
        }

        if (type === 'text_delta' && typeof event.content === 'string') {
          assistantRawText = mergeTextDelta(assistantRawText, event.content);
          applyAssistantText(assistantRawText, 'streaming');
          return;
        }

        if (
          (type === 'reasoning_delta' || type === 'thinking_delta' || type === 'reasoning_message_content') &&
          typeof event.content === 'string'
        ) {
          reasoningText = mergeTextDelta(reasoningText, event.content);
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            reasoning: reasoningText,
            reasoningStatus: 'streaming',
            status: 'streaming',
          });
          return;
        }

        if (type === 'reasoning_done' || type === 'thinking_done') {
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            reasoning: reasoningText,
            reasoningStatus: 'complete',
            status: 'streaming',
          });
          return;
        }

        if (type === 'text_done' && typeof event.full_text === 'string') {
          assistantRawText = event.full_text;
          applyAssistantText(assistantRawText, 'streaming');
          return;
        }

        if (type === 'turn_complete' && typeof event.assistant_text === 'string') {
          assistantRawText = event.assistant_text;
          applyAssistantText(assistantRawText, lastStatus);
          return;
        }

        if (type === 'error') {
          const message = typeof event.message === 'string' ? event.message : 'Astra stream failed.';
          assistantText = assistantText || message;
          lastStatus = 'failed';
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            content: assistantText,
            status: 'failed',
          });
          return;
        }

        if (type === 'run_finished') {
          const status = typeof event.status === 'string' ? event.status : 'completed';
          if (status === 'failed' || status === 'cancelled') {
            const message = typeof event.error === 'string' ? event.error : assistantText;
            assistantText = message || assistantText;
            lastStatus = 'failed';
          } else {
            lastStatus = 'complete';
          }
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            content: assistantText,
            reasoning: reasoningText || undefined,
            reasoningStatus: lastStatus === 'complete' ? 'complete' : (reasoningText ? 'complete' : undefined),
            status: lastStatus,
          });
        }
      };

      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) {
            break;
          }
          controller.enqueue(value);
          buffer += decoder.decode(value, { stream: true });

          const frames = buffer.split(/\r?\n\r?\n/);
          buffer = frames.pop() ?? '';
          for (const frame of frames) {
            const event = eventFromSseFrame(frame);
            if (event) {
              applyEvent(event);
            }
          }
        }

        const tail = decoder.decode();
        if (tail) {
          buffer += tail;
        }
        if (buffer.trim()) {
          const event = eventFromSseFrame(buffer);
          if (event) {
            applyEvent(event);
          }
        }

        if (lastStatus === 'streaming') {
          lastStatus = assistantText ? 'complete' : 'failed';
          updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
            content: assistantText || 'Astra completed the run without returning visible text.',
            reasoning: reasoningText || undefined,
            reasoningStatus: lastStatus === 'complete' ? 'complete' : undefined,
            status: lastStatus,
          });
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : 'Astra stream failed.';
        updateStreamingAssistantMessage(chatId, started.assistantMessage.id, {
          content: assistantText || message,
          status: 'failed',
        });
        controller.enqueue(sseFrame({ type: 'error', message }));
      } finally {
        controller.close();
      }
    },
  });

  return new Response(stream, {
    headers: {
      'Content-Type': 'text/event-stream; charset=utf-8',
      'Cache-Control': 'no-store, no-transform',
      Connection: 'keep-alive',
    },
  });
}
