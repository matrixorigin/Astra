'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import { SSEClient } from '@/lib/streaming/sse-client';
import type { ConnectionState, StreamEvent } from '@/lib/streaming/types';
import {
  applyRunEventsTransaction,
  SSE_CLIENT_DEAD_TIMEOUT_MS,
  subscribeWatermarks,
} from '@/lib/session-cache/indexeddb';

const MAX_EVENTS = 500;

type UseRunStreamOptions = {
  runId: string;
  /** Resume from this event index (for pagination). */
  lastIndex?: number;
  /** Auto-connect on mount. Defaults to true. */
  autoConnect?: boolean;
};

type UseRunStreamReturn = {
  events: Array<StreamEvent & { content?: string }>;
  connectionState: ConnectionState;
  connect: () => void;
  disconnect: () => void;
  clearEvents: () => void;
};

export function useRunStream({
  runId,
  lastIndex = 0,
  autoConnect = true,
}: UseRunStreamOptions): UseRunStreamReturn {
  const [events, setEvents] = useState<Array<StreamEvent & { content?: string }>>([]);
  const [connectionState, setConnectionState] = useState<ConnectionState>('disconnected');
  const clientRef = useRef<SSEClient | null>(null);
  const resumeIndexRef = useRef(lastIndex);
  const lastOkIdxRef = useRef(lastIndex - 1);
  const connectRef = useRef<() => void>(() => {});
  const streamKeyRef = useRef(`${runId}:${lastIndex}`);

  const handleEvent = useCallback((event: StreamEvent) => {
    if (typeof event.index === 'number') {
      resumeIndexRef.current = Math.max(resumeIndexRef.current, event.index + 1);
    } else {
      resumeIndexRef.current += 1;
    }

    setEvents((prev) => {
      const next = [...prev, event];
      return next.length > MAX_EVENTS ? next.slice(-MAX_EVENTS) : next;
    });

    const sessionId =
      event.type === 'session_info' ? event.session_id : `run-session-${runId}`;
    if (typeof event.index === 'number') {
      void applyRunEventsTransaction(sessionId, runId, [event], lastOkIdxRef.current).then(
        (result) => {
          lastOkIdxRef.current = result.lastOkIdx;
          if (result.gapDetected) {
            resumeIndexRef.current = result.reconnectLastIndex;
            clientRef.current?.close();
            setTimeout(() => connectRef.current(), 0);
          }
        },
      );
    }
  }, [runId]);

  useEffect(() => {
    const key = `${runId}:${lastIndex}`;
    if (streamKeyRef.current !== key) {
      streamKeyRef.current = key;
      resumeIndexRef.current = lastIndex;
      lastOkIdxRef.current = lastIndex - 1;
    }
  }, [runId, lastIndex]);

  const connect = useCallback(() => {
    clientRef.current?.close();

    const url = new URL(`/api/backend/chat/runs/${runId}/stream`, window.location.origin);
    if (resumeIndexRef.current > 0) {
      url.searchParams.set('last_index', String(resumeIndexRef.current));
    }

    const client = new SSEClient({
      url: url.toString(),
      onEvent: handleEvent,
      onStateChange: setConnectionState,
      heartbeatTimeoutMs: SSE_CLIENT_DEAD_TIMEOUT_MS,
    } as ConstructorParameters<typeof SSEClient>[0] & { heartbeatTimeoutMs: number });

    clientRef.current = client;
    void client.connect();
  }, [runId, lastIndex, handleEvent]);

  useEffect(() => {
    connectRef.current = connect;
  }, [connect]);

  const disconnect = useCallback(() => {
    clientRef.current?.close();
    clientRef.current = null;
  }, []);

  const clearEvents = useCallback(() => {
    resumeIndexRef.current = lastIndex;
    setEvents([]);
  }, [lastIndex]);

  useEffect(() => {
    if (autoConnect) {
      connect();
    }

    return () => {
      clientRef.current?.close();
      clientRef.current = null;
    };
  }, [autoConnect, connect]);

  useEffect(() => {
    return subscribeWatermarks((message) => {
      if (message.sessionId === `run-session-${runId}`) {
        lastOkIdxRef.current = Math.max(
          lastOkIdxRef.current,
          message.runEventHighWatermark,
        );
      }
    });
  }, [runId]);

  return { events, connectionState, connect, disconnect, clearEvents };
}
