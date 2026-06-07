import { NextRequest, NextResponse } from 'next/server';
import { AstraApiError, PATH_CHAT_STREAM, chatRunStreamPath, type RuntimeArtifactResponse } from '@astra/sdk';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import {
  beginStreamingMessage,
  getChatHydrated,
  resolveBackendModelName,
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from '@/lib/api/web-store';
import {
  WebRuntimeClient,
  readRuntimeErrorDetail,
  requireRuntimeClient,
} from '@/lib/runtime-client';
import type { ChatArtifactRef, SendMessageRequest } from '@/lib/api/types';

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

function normalizedActiveSkills(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }
  return [...new Set(skills.map((skill) => skill.trim()).filter(Boolean))].sort((left, right) => (
    left.localeCompare(right)
  ));
}

function stringField(value: unknown) {
  return typeof value === 'string' && value.trim() ? value : null;
}

function numberField(value: unknown) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

const INTERNAL_ARTIFACT_KINDS = new Set(['composite_snapshot_index']);
const INTERNAL_ARTIFACT_SOURCES = new Set(['composite_snapshot_index']);
const CHAT_VISIBLE_ARTIFACT_SOURCES = new Set(['publish_artifact']);
const CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS = new Set(['artifact_file_v1']);

function isChatVisibleRuntimeArtifact(
  source: string | null,
  kind: string,
  metadata: Record<string, unknown> | null,
) {
  if (INTERNAL_ARTIFACT_KINDS.has(kind) || (source && INTERNAL_ARTIFACT_SOURCES.has(source))) {
    return false;
  }

  const normalizeVersion = stringField(metadata?.normalize_version);
  return Boolean(
    source
      && CHAT_VISIBLE_ARTIFACT_SOURCES.has(source)
      && normalizeVersion
      && CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS.has(normalizeVersion),
  );
}

function artifactFromRuntime(artifact: RuntimeArtifactResponse): ChatArtifactRef | null {
  const content = artifact.content && typeof artifact.content === 'object'
    ? artifact.content as Record<string, unknown>
    : null;
  const metadata = artifact.metadata && typeof artifact.metadata === 'object'
    ? artifact.metadata
    : null;
  const id = stringField(artifact.artifact_id);
  const kind = stringField(artifact.artifact_kind) ?? stringField(content?.kind);
  const source = stringField(artifact.source);
  if (!id || !kind || !content) {
    return null;
  }
  if (!isChatVisibleRuntimeArtifact(source, kind, metadata)) {
    return null;
  }
  return {
    id,
    kind,
    source,
    title: stringField(content.title) ?? stringField(metadata?.title) ?? stringField(content.filename),
    filename: stringField(content.filename) ?? stringField(metadata?.download_filename),
    sizeBytes: numberField(content.byte_size) ?? numberField(metadata?.byte_size),
    contentType: stringField(content.content_type) ?? stringField(metadata?.content_type),
    renderer: stringField(content.renderer) ?? stringField(metadata?.renderer),
    downloadFilename: stringField(metadata?.download_filename),
    content,
    createdAt: artifact.created_at ?? null,
  };
}

async function fetchSessionArtifacts(client: WebRuntimeClient, sessionId: string) {
  const body = await client.sdk.listSessionArtifacts(sessionId, { limit: 50 });
  return (body.artifacts ?? [])
    .map(artifactFromRuntime)
    .filter((artifact): artifact is ChatArtifactRef => Boolean(artifact));
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
  return readRuntimeErrorDetail(response);
}

function isRuntimeSessionNotFound(error: unknown) {
  return error instanceof AstraApiError && error.status === 404;
}

function lastAssistantMessageId(messages: Array<{ id: string; role: string }>) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    if (messages[index]?.role === 'assistant') {
      return messages[index]?.id ?? null;
    }
  }
  return null;
}

function proxyRunStream(params: {
  backendResponse: Response;
  ownerUserId: string;
  chatId: string;
  sessionId: string;
  runtime: WebRuntimeClient;
  assistantMessageId: string;
  knownArtifactIds: Set<string>;
  localMessages?: {
    userMessage: unknown;
    assistantMessage: unknown;
  };
}) {
  const {
    backendResponse,
    ownerUserId,
    chatId,
    sessionId,
    runtime,
    assistantMessageId,
    knownArtifactIds,
    localMessages,
  } = params;

  let assistantText = '';
  let assistantRawText = '';
  let reasoningText = '';
  let lastStatus: 'streaming' | 'complete' | 'failed' = 'streaming';
  let protocolError = false;
  let runLifecycle: 'running' | 'paused' | 'finished' = 'running';

  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      const reader = backendResponse.body?.getReader();
      if (!reader) {
        controller.enqueue(sseFrame({ type: 'error', message: 'Astra stream body is unavailable.' }));
        controller.close();
        return;
      }

      if (localMessages) {
        controller.enqueue(sseFrame({
          type: 'local_messages',
          user_message: localMessages.userMessage,
          assistant_message: localMessages.assistantMessage,
        }));
      }

      const decoder = new TextDecoder();
      let buffer = '';

      const applyEvent = (event: Record<string, unknown>) => {
        const type = typeof event.type === 'string' ? event.type : '';
        if (protocolError) {
          return;
        }

        const applyAssistantText = (rawText: string, status: 'streaming' | 'complete' | 'failed') => {
          const split = splitThinkingTags(rawText);
          assistantText = split.visibleText;
          if (split.hasThinking) {
            reasoningText = split.reasoning;
          }
          updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
            content: assistantText,
            reasoning: reasoningText || undefined,
            reasoningStatus: reasoningText
              ? (split.reasoningOpen ? 'streaming' : 'complete')
              : (status === 'streaming' ? 'streaming' : (status === 'complete' ? 'complete' : undefined)),
            status,
          });
        };

        if (type === 'session_info' && typeof event.session_id === 'string') {
          if (event.session_id !== sessionId) {
            const message = `Runtime returned session_id ${event.session_id}, but Web chat is bound to ${sessionId}.`;
            protocolError = true;
            assistantText = message;
            lastStatus = 'failed';
            updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
              content: message,
              status: 'failed',
            });
            controller.enqueue(sseFrame({ type: 'error', message }));
          }
          if (typeof event.run_id === 'string') {
            setChatActiveRun(ownerUserId, chatId, {
              runId: event.run_id,
              status: 'running',
              waitingFor: null,
            });
          }
          return;
        }

        if (type === 'run_started' && typeof event.run_id === 'string') {
          runLifecycle = 'running';
          setChatActiveRun(ownerUserId, chatId, {
            runId: event.run_id,
            status: 'running',
            waitingFor: null,
          });
          return;
        }

        if (type === 'run_paused' && typeof event.run_id === 'string') {
          runLifecycle = 'paused';
          setChatActiveRun(ownerUserId, chatId, {
            runId: event.run_id,
            status: 'paused',
            waitingFor: null,
          });
          return;
        }

        if (type === 'run_resumed' && typeof event.run_id === 'string') {
          runLifecycle = 'running';
          setChatActiveRun(ownerUserId, chatId, {
            runId: event.run_id,
            status: 'running',
            waitingFor: null,
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
          updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
            reasoning: reasoningText,
            reasoningStatus: 'streaming',
            status: 'streaming',
          });
          return;
        }

        if (type === 'reasoning_done' || type === 'thinking_done') {
          updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
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
          updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
            content: assistantText,
            status: 'failed',
          });
          return;
        }

        if (type === 'run_finished') {
          const status = typeof event.status === 'string' ? event.status : 'completed';
          runLifecycle = 'finished';
          setChatActiveRun(ownerUserId, chatId, undefined);
          if (status === 'failed' || status === 'cancelled') {
            const message = typeof event.error === 'string' ? event.error : assistantText;
            assistantText = message || assistantText;
            lastStatus = 'failed';
          } else {
            lastStatus = 'complete';
          }
          updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
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
          if (runLifecycle === 'paused') {
            updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
              content: assistantText,
              reasoning: reasoningText || undefined,
              reasoningStatus: reasoningText ? 'streaming' : undefined,
              status: 'streaming',
            });
          } else {
            lastStatus = assistantText ? 'complete' : 'failed';
            setChatActiveRun(ownerUserId, chatId, undefined);
            updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
              content: assistantText || 'Astra completed the run without returning visible text.',
              reasoning: reasoningText || undefined,
              reasoningStatus: lastStatus === 'complete' ? 'complete' : undefined,
              status: lastStatus,
            });
          }
        }

        if (lastStatus === 'complete') {
          const artifacts = (await fetchSessionArtifacts(runtime, sessionId))
            .filter((artifact) => !knownArtifactIds.has(artifact.id));
          if (artifacts.length > 0) {
            updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
              artifacts,
            });
            controller.enqueue(sseFrame({ type: 'artifacts', artifacts }));
          }
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : 'Astra stream failed.';
        setChatActiveRun(ownerUserId, chatId, undefined);
        updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
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

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const ownerUserId = auth.user.user_id;
  const { chatId } = await context.params;
  const body = (await request.json()) as SendMessageRequest;
  if (!body.content?.trim()) {
    return NextResponse.json({ error: 'content is required' }, { status: 400 });
  }

  const chat = await getChatHydrated(ownerUserId, chatId);
  if (!chat) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  if (chat.chat.archivedAt) {
    return NextResponse.json({ error: 'archived chat is read-only' }, { status: 409 });
  }

  let runtime: WebRuntimeClient;
  try {
    runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'stream web chat turn',
    });
  } catch {
    return NextResponse.json({ error: 'AUTH_REQUIRED' }, { status: 401 });
  }

  try {
    await runtime.sdk.getRuntimeSession(chatId);
  } catch (error) {
    if (isRuntimeSessionNotFound(error)) {
      return NextResponse.json({ error: `session not found: ${chatId}` }, { status: 404 });
    }
    const message = error instanceof Error ? error.message : 'Failed to verify runtime session.';
    return NextResponse.json({ error: message }, { status: 502 });
  }

  const model = await resolveBackendModelName(runtime, body.options?.model);
  const activeSkills = normalizedActiveSkills(body.options?.activeSkills);
  const sessionId = chatId;
  const knownArtifactIds = new Set<string>();
  try {
    const existingArtifacts = await fetchSessionArtifacts(runtime, sessionId);
    for (const artifact of existingArtifacts) {
      knownArtifactIds.add(artifact.id);
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : 'Failed to load artifacts.';
    return NextResponse.json({ error: message }, { status: 502 });
  }

  const started = beginStreamingMessage(ownerUserId, chatId, body);
  if (!started) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }
  const backendResponse = await runtime.fetchResponse(PATH_CHAT_STREAM, {
    method: 'POST',
    auth: 'required',
    operation: 'stream web chat turn',
    json: {
      message: body.content,
      session_id: sessionId,
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
    },
  });

  if (!backendResponse.ok || !backendResponse.body) {
    const detail = await readErrorDetail(backendResponse);
    updateStreamingAssistantMessage(ownerUserId, chatId, started.assistantMessage.id, {
      content: detail,
      status: 'failed',
    });
    return NextResponse.json({ error: detail }, { status: backendResponse.status || 502 });
  }
  return proxyRunStream({
    backendResponse,
    ownerUserId,
    chatId,
    sessionId,
    runtime,
    assistantMessageId: started.assistantMessage.id,
    knownArtifactIds,
    localMessages: {
      userMessage: started.userMessage,
      assistantMessage: started.assistantMessage,
    },
  });
}

export async function GET(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const ownerUserId = auth.user.user_id;
  const { chatId } = await context.params;
  const runId = request.nextUrl.searchParams.get('runId')?.trim();
  if (!runId) {
    return NextResponse.json({ error: 'runId is required' }, { status: 400 });
  }

  const chat = await getChatHydrated(ownerUserId, chatId);
  if (!chat) {
    return NextResponse.json({ error: 'chat not found' }, { status: 404 });
  }

  const assistantMessageId = lastAssistantMessageId(chat.messages);
  if (!assistantMessageId) {
    return NextResponse.json({ error: 'no assistant message is available to resume' }, { status: 409 });
  }

  let runtime: WebRuntimeClient;
  try {
    runtime = await requireRuntimeClient({
      auth: 'required',
      operation: `stream existing web run ${runId}`,
    });
  } catch {
    return NextResponse.json({ error: 'AUTH_REQUIRED' }, { status: 401 });
  }

  const sessionId = chatId;
  const knownArtifactIds = new Set<string>();
  try {
    const existingArtifacts = await fetchSessionArtifacts(runtime, sessionId);
    for (const artifact of existingArtifacts) {
      knownArtifactIds.add(artifact.id);
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : 'Failed to load artifacts.';
    return NextResponse.json({ error: message }, { status: 502 });
  }

  const backendResponse = await runtime.fetchResponse(chatRunStreamPath(runId), {
    method: 'GET',
    auth: 'required',
    operation: `stream existing web run ${runId}`,
  });

  if (!backendResponse.ok || !backendResponse.body) {
    const detail = await readErrorDetail(backendResponse);
    updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
      content: detail,
      status: 'failed',
    });
    return NextResponse.json({ error: detail }, { status: backendResponse.status || 502 });
  }

  return proxyRunStream({
    backendResponse,
    ownerUserId,
    chatId,
    sessionId,
    runtime,
    assistantMessageId,
    knownArtifactIds,
  });
}
