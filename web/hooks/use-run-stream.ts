'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import { SSEClient } from '@/lib/streaming/sse-client';
import type { ConnectionState, StreamEvent } from '@/lib/streaming/types';

const MAX_EVENTS = 500;

type UseRunStreamOptions = {
  apiUrl: string;
  token: string;
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
  apiUrl,
  token,
  runId,
  lastIndex = 0,
  autoConnect = true,
}: UseRunStreamOptions): UseRunStreamReturn {
  const [events, setEvents] = useState<StreamEvent[]>([]);
  const [connectionState, setConnectionState] = useState<ConnectionState>('disconnected');
  const clientRef = useRef<SSEClient | null>(null);

  const handleEvent = useCallback((event: StreamEvent) => {
    setEvents((prev) => {
      const next = [...prev, event];
      return next.length > MAX_EVENTS ? next.slice(-MAX_EVENTS) : next;
    });
  }, []);

  const connect = useCallback(() => {
    clientRef.current?.close();

    const url = new URL(`/chat/runs/${runId}/stream`, apiUrl);
    if (lastIndex > 0) {
      url.searchParams.set('last_index', String(lastIndex));
    }

    const client = new SSEClient({
      url: url.toString(),
      token,
      onEvent: handleEvent,
      onStateChange: setConnectionState,
    });

    clientRef.current = client;
    void client.connect();
  }, [apiUrl, token, runId, lastIndex, handleEvent]);

  const disconnect = useCallback(() => {
    clientRef.current?.close();
    clientRef.current = null;
  }, []);

  const clearEvents = useCallback(() => {
    setEvents([]);
  }, []);

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
