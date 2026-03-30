'use client';

import { useRunStream } from '@/hooks/use-run-stream';
import { LiveRunPanel } from '@/components/streaming/live-run-panel';

export function RunStreamViewer({
  apiUrl,
  token,
  runId,
  title,
}: {
  apiUrl: string;
  token: string;
  runId: string;
  title?: string;
}) {
  const { events, connectionState, connect, disconnect } = useRunStream({
    apiUrl,
    token,
    runId,
  });

  return (
    <LiveRunPanel
      events={events}
      connectionState={connectionState}
      onReconnect={connect}
      onDisconnect={disconnect}
      title={title ?? `Run ${runId}`}
    />
  );
}
