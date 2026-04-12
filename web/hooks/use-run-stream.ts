'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import { SSEClient } from '@/lib/streaming/sse-client';
import type { ConnectionState, StreamEvent } from '@/lib/streaming/types';

const MAX_EVENTS = 500;

type UseRunStreamOptions = {
  runId: string;
  /** Resume from this event index (for pagination). */
  lastIndex?: number;
  /** Auto-connect on mount. Defaults to true. */
  autoConnect?: boolean;
};

type UseRunStreamReturn = {
  events: StreamEvent[];
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
  const [events, setEvents] = useState<StreamEvent[]>([]);
  const [connectionState, setConnectionState] = useState<ConnectionState>('disconnected');
  const clientRef = useRef<SSEClient | null>(null);
  const resumeIndexRef = useRef(lastIndex);

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
  }, []);

  useEffect(() => {
    resumeIndexRef.current = lastIndex;
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
    });

    clientRef.current = client;
    void client.connect();
  }, [runId, lastIndex, handleEvent]);

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

  return { events, connectionState, connect, disconnect, clearEvents };
}
