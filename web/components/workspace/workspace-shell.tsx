'use client';

import { useState, useCallback } from 'react';
import { useChatStream } from '@/hooks/use-chat-stream';
import { ChatThread } from './chat-thread';
import { ChatInput } from './chat-input';
import { ToolTimeline } from './tool-timeline';
import { SessionSidebar } from './session-sidebar';
import { PlanProgressPanel } from './plan-progress';
import { TokenUsageBar } from './token-usage-bar';
import type { ChatConfig } from '@/lib/workspace/types';
import type { SessionSummary, EventSummary } from '@/lib/models/platform';
import { ConnectionStatus } from '@/components/streaming/connection-status';
import { EventLogViewer } from '@/components/events/event-log-viewer';
import { AgentTree } from '@/components/agents/agent-tree';

type SidePanel = 'tools' | 'plan' | 'agents' | 'events' | 'context';

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
  const [activeConfig, setActiveConfig] = useState<ChatConfig>(config);
  const chat = useChatStream(activeConfig);
  const [sidePanel, setSidePanel] = useState<SidePanel>('tools');
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);

  const displaySessionId = chat.sessionId ?? activeConfig.sessionId ?? null;

  const handleSelectSession = useCallback(
    (sessionId: string) => {
      chat.reset();
      setActiveConfig((prev) => ({ ...prev, sessionId }));
      // Update URL without full reload
      const url = new URL(window.location.href);
      url.searchParams.set('sessionId', sessionId);
      window.history.pushState({}, '', url.toString());
    },
    [chat],
  );

  const handleNewSession = useCallback(() => {
    chat.reset();
    setActiveConfig((prev) => ({ ...prev, sessionId: undefined }));
    const url = new URL(window.location.href);
    url.searchParams.delete('sessionId');
    window.history.pushState({}, '', url.toString());
  }, [chat]);

  // Auto-switch to plan tab when plan appears, or agents tab when agent events arrive
  const effectiveSidePanel =
    sidePanel === 'tools' && chat.plan && chat.toolCalls.length === 0
      ? 'plan'
      : sidePanel === 'tools' && chat.agentEvents.length > 0 && chat.toolCalls.length === 0
        ? 'agents'
        : sidePanel;

  return (
    <div className="flex h-[calc(100vh-8rem)] overflow-hidden rounded-2xl border border-slate-800">
      {/* Session sidebar */}
      <SessionSidebar
        currentSessionId={displaySessionId}
        onSelectSession={handleSelectSession}
        onNewSession={handleNewSession}
        collapsed={sidebarCollapsed}
        onToggle={() => setSidebarCollapsed(!sidebarCollapsed)}
      />

      {/* Chat column */}
      <div className="flex min-w-0 flex-1 flex-col">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-slate-800 bg-slate-950/80 px-4 py-3">
          <div className="flex items-center gap-3">
            <h2 className="text-sm font-medium text-white">Agent workspace</h2>
            <ConnectionStatus
              state={
                chat.connectionState === 'streaming'
                  ? 'connected'
                  : chat.connectionState === 'error'
                    ? 'error'
                    : 'disconnected'
              }
            />
          </div>
          <div className="flex items-center gap-3 text-xs text-slate-500">
            {displaySessionId ? (
              <span className="max-w-[120px] truncate" title={displaySessionId}>
                {displaySessionId.slice(0, 8)}…
              </span>
            ) : null}
            {chat.runId ? (
              <span className="max-w-[120px] truncate" title={chat.runId}>
                run: {chat.runId.slice(0, 8)}…
              </span>
            ) : null}
            <button
              type="button"
              onClick={handleNewSession}
              className="rounded-md px-2 py-1 text-slate-400 hover:bg-slate-800 hover:text-white"
            >
              New
            </button>
            <button
              type="button"
              onClick={chat.reset}
              className="rounded-md px-2 py-1 text-slate-400 hover:bg-slate-800 hover:text-white"
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

        {/* Token usage */}
        <TokenUsageBar usage={chat.usage} />

        {/* Input */}
        <ChatInput
          onSend={chat.sendMessage}
          onStop={chat.stop}
          disabled={chat.isStreaming}
          isStreaming={chat.isStreaming}
          placeholder={
            displaySessionId
              ? 'Send a message…'
              : 'Send a message to start a new session…'
          }
        />
      </div>

      {/* Side panel */}
      <div className="hidden w-96 flex-col border-l border-slate-800 lg:flex">
        {/* Panel tabs */}
        <div className="flex border-b border-slate-800 bg-slate-950/80">
          {(
            [
              { key: 'tools' as const, label: 'Tools', count: chat.toolCalls.length },
              { key: 'plan' as const, label: 'Plan', count: chat.plan?.subtasks.length ?? 0 },
              { key: 'agents' as const, label: 'Agents', count: chat.agentEvents.filter((e) => e.type === 'agent_spawned').length },
              { key: 'events' as const, label: 'Events', count: events?.length ?? 0 },
              { key: 'context' as const, label: 'Context' },
            ]
          ).map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setSidePanel(tab.key)}
              className={`flex-1 px-2 py-3 text-xs font-medium ${
                effectiveSidePanel === tab.key
                  ? 'border-b-2 border-sky-500 text-sky-300'
                  : 'text-slate-400 hover:text-slate-200'
              }`}
            >
              {tab.label}
              {'count' in tab && (tab.count ?? 0) > 0 ? (
                <span className="ml-1 rounded-full bg-slate-800 px-1.5 py-0.5 text-[10px]">
                  {tab.count}
                </span>
              ) : null}
            </button>
          ))}
        </div>

        {/* Panel content */}
        <div className="flex-1 overflow-y-auto">
          {effectiveSidePanel === 'tools' ? (
            <ToolTimeline toolCalls={chat.toolCalls} />
          ) : effectiveSidePanel === 'plan' ? (
            chat.plan ? (
              <PlanProgressPanel plan={chat.plan} />
            ) : (
              <div className="flex flex-col items-center justify-center p-6 text-center">
                <p className="text-xs text-slate-500">
                  No active plan. Plans will appear when the agent creates one.
                </p>
              </div>
            )
          ) : effectiveSidePanel === 'agents' ? (
            <div className="p-4">
              {chat.agentEvents.length > 0 ? (
                <AgentTree events={chat.agentEvents} />
              ) : (
                <div className="flex flex-col items-center justify-center p-6 text-center">
                  <p className="text-xs text-slate-500">
                    No agents spawned yet. Multi-agent activity will appear here.
                  </p>
                </div>
              )}
            </div>
          ) : effectiveSidePanel === 'events' ? (
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
