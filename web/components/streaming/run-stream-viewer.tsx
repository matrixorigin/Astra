'use client';

import { useRunStream } from '@/hooks/use-run-stream';
import { LiveRunPanel } from '@/components/streaming/live-run-panel';

export function RunStreamViewer({
  runId,
  title,
}: {
  runId: string;
  title?: string;
}) {
  const { events, connectionState, connect, disconnect } = useRunStream({
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
