'use client';

import { useCallback, useRef, useState } from 'react';
import type { ChatConfig, ChatMessage, ToolCall, WorkspaceState } from '@/lib/workspace/types';
import type { StreamEvent } from '@/lib/streaming/types';

let nextId = 0;
function uid(): string {
  return `msg_${Date.now()}_${++nextId}`;
}

type UseChatStreamReturn = WorkspaceState & {
  sendMessage: (content: string) => void;
  reset: () => void;
};

/**
 * Hook that manages a streaming chat conversation via POST /chat/stream (SSE).
 * Each call to `sendMessage` adds a user message, opens an SSE connection, and
 * incrementally builds the assistant response.
 */
export function useChatStream(config: ChatConfig): UseChatStreamReturn {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [toolCalls, setToolCalls] = useState<ToolCall[]>([]);
  const [sessionId, setSessionId] = useState<string | null>(config.sessionId ?? null);
  const [runId, setRunId] = useState<string | null>(null);
  const [isStreaming, setIsStreaming] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Refs for mutable state during stream processing
  const controllerRef = useRef<AbortController | null>(null);
  const assistantIdRef = useRef<string>('');
  const accumulatedTextRef = useRef('');
  const toolCallMapRef = useRef<Map<string, ToolCall>>(new Map());

  const processEvent = useCallback(
    (event: StreamEvent) => {
      switch (event.type) {
        case 'session_info': {
          const sid = event.session_id;
          const rid = event.run_id ?? null;
          setSessionId(sid);
          setRunId(rid);
          break;
        }

        case 'text_delta': {
          accumulatedTextRef.current += event.content;
          const text = accumulatedTextRef.current;
          const id = assistantIdRef.current;
          setMessages((prev) =>
            prev.map((m) => (m.id === id ? { ...m, content: text } : m)),
          );
          break;
        }

        case 'tool_call_start': {
          const tc: ToolCall = {
            callId: event.call_id,
            tool: event.tool,
            arguments: event.arguments,
            status: 'running',
            startedAt: Date.now(),
          };
          toolCallMapRef.current.set(event.call_id, tc);
          setToolCalls(Array.from(toolCallMapRef.current.values()));
          break;
        }

        case 'tool_call_end': {
          const existing = toolCallMapRef.current.get(event.call_id);
          if (existing) {
            const updated = {
              ...existing,
              result: event.result,
              status: 'done' as const,
              finishedAt: Date.now(),
            };
            toolCallMapRef.current.set(event.call_id, updated);
            setToolCalls(Array.from(toolCallMapRef.current.values()));
          }
          break;
        }

        case 'turn_complete': {
          const id = assistantIdRef.current;
          const finalTools = Array.from(toolCallMapRef.current.values());
          setMessages((prev) =>
            prev.map((m) =>
              m.id === id
                ? { ...m, streaming: false, toolCalls: finalTools.length > 0 ? finalTools : undefined }
                : m,
            ),
          );
          setIsStreaming(false);
          break;
        }

        case 'error': {
          setError(event.message);
          setIsStreaming(false);
          break;
        }

        case 'usage':
        case 'warning':
        case 'explain':
          // Non-critical — no UI update needed
          break;
      }
    },
    [],
  );

  const sendMessage = useCallback(
    (content: string) => {
      if (isStreaming) return;

      setError(null);
      setIsStreaming(true);

      // Add user message
      const userMsg: ChatMessage = {
        id: uid(),
        role: 'user',
        content,
        timestamp: Date.now(),
      };

      // Prepare placeholder assistant message
      const assistantMsg: ChatMessage = {
        id: uid(),
        role: 'assistant',
        content: '',
        timestamp: Date.now(),
        streaming: true,
      };
      assistantIdRef.current = assistantMsg.id;
      accumulatedTextRef.current = '';
      toolCallMapRef.current.clear();
      setToolCalls([]);

      setMessages((prev) => [...prev, userMsg, assistantMsg]);

      // Open SSE connection
      const controller = new AbortController();
      controllerRef.current = controller;

      const body = JSON.stringify({
        message: content,
        session_id: sessionId,
        agent_id: config.agentId ?? undefined,
        model: config.model ?? undefined,
      });

      fetch(new URL('/chat/stream', config.apiUrl).toString(), {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${config.token}`,
          'Content-Type': 'application/json',
          Accept: 'text/event-stream',
        },
        body,
        signal: controller.signal,
      })
        .then(async (response) => {
          if (!response.ok) {
            const text = await response.text().catch(() => '');
            throw new Error(`Chat request failed: ${response.status} ${text}`);
          }

          if (!response.body) {
            throw new Error('No response body');
          }

          const reader = response.body.getReader();
          const decoder = new TextDecoder();
          let buffer = '';

          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;

            buffer += decoder.decode(value, { stream: true });
            const parts = buffer.split('\n\n');
            buffer = parts.pop() ?? '';

            for (const part of parts) {
              for (const line of part.split('\n')) {
                const trimmed = line.trim();
                if (trimmed.startsWith('data: ')) {
                  try {
                    const event = JSON.parse(trimmed.slice(6)) as StreamEvent;
                    processEvent(event);
                  } catch {
                    // Non-JSON data line
                  }
                }
              }
            }
          }

          // Stream ended — mark complete if not already
          setIsStreaming(false);
        })
        .catch((err) => {
          if (err instanceof DOMException && err.name === 'AbortError') return;
          setError(err instanceof Error ? err.message : 'Unknown streaming error');
          setIsStreaming(false);
        });
    },
    [isStreaming, sessionId, config, processEvent],
  );

  const reset = useCallback(() => {
    controllerRef.current?.abort();
    controllerRef.current = null;
    setMessages([]);
    setToolCalls([]);
    setSessionId(config.sessionId ?? null);
    setRunId(null);
    setIsStreaming(false);
    setError(null);
    accumulatedTextRef.current = '';
    toolCallMapRef.current.clear();
  }, [config.sessionId]);

  return {
    sessionId,
    runId,
    messages,
    toolCalls,
    isStreaming,
    error,
    sendMessage,
    reset,
  };
}
