'use client';

import { useState } from 'react';
import { useChatStream } from '@/hooks/use-chat-stream';
import { ChatThread } from './chat-thread';
import { ChatInput } from './chat-input';
import { ToolTimeline } from './tool-timeline';
import type { ChatConfig } from '@/lib/workspace/types';
import type { SessionSummary, EventSummary } from '@/lib/models/platform';
import { ConnectionStatus } from '@/components/streaming/connection-status';
import { EventLogViewer } from '@/components/events/event-log-viewer';

type SidePanel = 'tools' | 'events' | 'context';

export function WorkspaceShell({
  config,
  session,
  events,
  reflection,
}: {
  config: ChatConfig;
  session?: SessionSummary;
  events?: EventSummary[];
  reflection?: string;
}) {
  const chat = useChatStream(config);
  const [sidePanel, setSidePanel] = useState<SidePanel>('tools');

  const displaySessionId = chat.sessionId ?? config.sessionId ?? null;

  return (
    <div className="flex h-[calc(100vh-8rem)] flex-col overflow-hidden rounded-2xl border border-slate-800 lg:flex-row">
      {/* Chat column */}
      <div className="flex min-w-0 flex-1 flex-col">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-slate-800 bg-slate-950/80 px-4 py-3">
          <div className="flex items-center gap-3">
            <h2 className="text-sm font-medium text-white">Agent workspace</h2>
            <ConnectionStatus
              state={chat.isStreaming ? 'connected' : 'disconnected'}
            />
          </div>
          <div className="flex items-center gap-3 text-xs text-slate-500">
            {displaySessionId ? <span>session: {displaySessionId}</span> : null}
            {chat.runId ? <span>run: {chat.runId}</span> : null}
            <button
              type="button"
              onClick={chat.reset}
              className="text-slate-400 hover:text-white"
            >
              Reset
            </button>
          </div>
        </div>

        {/* Error banner */}
        {chat.error ? (
          <div className="border-b border-red-800/50 bg-red-950/30 px-4 py-2 text-xs text-red-300">
            {chat.error}
          </div>
        ) : null}

        {/* Chat thread */}
        <ChatThread messages={chat.messages} />

        {/* Input */}
        <ChatInput
          onSend={chat.sendMessage}
          disabled={chat.isStreaming}
          placeholder={
            displaySessionId
              ? `Message session ${displaySessionId}…`
              : 'Send a message to start a new session…'
          }
        />
      </div>

      {/* Side panel */}
      <div className="flex w-full flex-col border-t border-slate-800 lg:w-96 lg:border-l lg:border-t-0">
        {/* Panel tabs */}
        <div className="flex border-b border-slate-800 bg-slate-950/80">
          {(
            [
              { key: 'tools', label: 'Tools', count: chat.toolCalls.length },
              { key: 'events', label: 'Events', count: events?.length ?? 0 },
              { key: 'context', label: 'Context' },
            ] as const
          ).map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setSidePanel(tab.key)}
              className={`flex-1 px-3 py-3 text-xs font-medium ${
                sidePanel === tab.key
                  ? 'border-b-2 border-sky-500 text-sky-300'
                  : 'text-slate-400 hover:text-slate-200'
              }`}
            >
              {tab.label}
              {'count' in tab && tab.count > 0 ? (
                <span className="ml-1.5 rounded-full bg-slate-800 px-1.5 py-0.5 text-[10px]">
                  {tab.count}
                </span>
              ) : null}
            </button>
          ))}
        </div>

        {/* Panel content */}
        <div className="flex-1 overflow-y-auto">
          {sidePanel === 'tools' ? (
            <ToolTimeline toolCalls={chat.toolCalls} />
          ) : sidePanel === 'events' ? (
            <div className="p-4">
              {events && events.length > 0 ? (
                <EventLogViewer events={events} emptyMessage="No events." />
              ) : (
                <p className="text-sm text-slate-500">No session events loaded.</p>
              )}
            </div>
          ) : (
            <div className="space-y-4 p-4">
              {session ? (
                <div className="rounded-xl border border-slate-800 p-3">
                  <p className="text-xs uppercase tracking-wide text-slate-500">Session</p>
                  <p className="mt-1 text-sm text-white">{session.title}</p>
                  <div className="mt-2 grid grid-cols-2 gap-2 text-xs text-slate-400">
                    <span>Status: {session.status}</span>
                    <span>Events: {session.eventCount}</span>
                    <span>Owner: {session.owner}</span>
                    <span>Agent: {session.agentId ?? 'n/a'}</span>
                  </div>
                </div>
              ) : (
                <p className="text-sm text-slate-500">
                  No session loaded. Send a message to create one.
                </p>
              )}

              {reflection ? (
                <div className="rounded-xl border border-slate-800 p-3">
                  <p className="text-xs uppercase tracking-wide text-slate-500">Reflection</p>
                  <p className="mt-2 text-sm leading-relaxed text-slate-300">{reflection}</p>
                </div>
              ) : null}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
