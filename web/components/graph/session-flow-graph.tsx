'use client';

import { useMemo, useState, useCallback } from 'react';
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  Panel,
  useNodesState,
  useEdgesState,
  type Node,
  type Edge,
  type ColorMode,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';

import type { EventSummary } from '@/lib/models/platform';

/* ── Layout constants ── */
const COL_SESSION = 0;
const COL_EVENT = 320;
const COL_AGENT = 640;
const ROW_H = 90;
const NODE_GAP = 16;

/* ── Color helpers ── */
const eventTypeColors: Record<string, { bg: string; border: string }> = {
  session_start:   { bg: '#052e16', border: '#22c55e' },
  session_end:     { bg: '#450a0a', border: '#ef4444' },
  turn:            { bg: '#0c4a6e', border: '#0ea5e9' },
  tool_call:       { bg: '#422006', border: '#f59e0b' },
  error:           { bg: '#450a0a', border: '#ef4444' },
  plan_progress:   { bg: '#1e1b4b', border: '#8b5cf6' },
  agent_delegated: { bg: '#2e1065', border: '#a78bfa' },
  agent_completed: { bg: '#052e16', border: '#22c55e' },
};
const defaultEventColor = { bg: '#0f172a', border: '#334155' };
const failedEventColor = { bg: '#450a0a', border: '#ef4444' };
const cancelledEventColor = { bg: '#2e1065', border: '#a78bfa' };

function getEventColor(event: EventSummary | string) {
  if (typeof event !== 'string' && event.type === 'agent_completed') {
    if (event.status === 'failed') return failedEventColor;
    if (event.status === 'cancelled') return cancelledEventColor;
  }

  const eventType = typeof event === 'string' ? event : event.type;
  const lower = eventType.toLowerCase();
  for (const [key, val] of Object.entries(eventTypeColors)) {
    if (lower.includes(key)) return val;
  }
  return defaultEventColor;
}

/* ── Build React Flow graph ── */
function buildSessionGraph(
  sessionId: string,
  sessionTitle: string,
  events: EventSummary[],
): { nodes: Node[]; edges: Edge[] } {
  const nodes: Node[] = [];
  const edges: Edge[] = [];

  // Session root node
  const sessionNodeId = `session-${sessionId}`;
  nodes.push({
    id: sessionNodeId,
    type: 'default',
    position: { x: COL_SESSION, y: events.length * ROW_H / 2 - 40 },
    data: {
      label: (
        <div className="text-left">
          <div className="text-xs font-semibold uppercase tracking-wide text-sky-400">Session</div>
          <div className="mt-0.5 text-sm font-medium text-white">{sessionTitle}</div>
          <div className="mt-0.5 text-[10px] text-slate-500">{sessionId.slice(0, 12)}…</div>
        </div>
      ),
    },
    style: {
      background: '#0c4a6e',
      border: '2px solid #0ea5e9',
      borderRadius: 12,
      padding: '10px 14px',
      width: 240,
    },
  });

  // Agent nodes (deduplicated)
  const agentIds = Array.from(
    new Set(events.map((e) => e.agentId).filter((a): a is string => Boolean(a))),
  );
  const agentNodeIds = new Map<string, string>();
  agentIds.forEach((agentId, i) => {
    const nodeId = `agent-${agentId}`;
    agentNodeIds.set(agentId, nodeId);
    nodes.push({
      id: nodeId,
      type: 'default',
      position: { x: COL_AGENT, y: i * (ROW_H + NODE_GAP) + 40 },
      data: {
        label: (
          <div className="text-left">
            <div className="text-xs font-semibold uppercase tracking-wide text-purple-400">Agent</div>
            <div className="mt-0.5 text-sm font-medium text-white">{agentId}</div>
          </div>
        ),
      },
      style: {
        background: '#1e1b4b',
        border: '2px dashed #8b5cf6',
        borderRadius: 12,
        padding: '10px 14px',
        width: 200,
      },
    });
  });

  // Event nodes
  const displayEvents = events.slice(0, 30); // Cap at 30 for performance
  let prevEventNodeId: string | null = null;

  displayEvents.forEach((event, i) => {
    const nodeId = `event-${event.id}`;
    const color = getEventColor(event);

    nodes.push({
      id: nodeId,
      type: 'default',
      position: { x: COL_EVENT, y: i * ROW_H },
      data: {
        label: (
          <div className="text-left">
            <div className="flex items-center gap-1.5">
              <div
                className="h-2 w-2 rounded-full"
                style={{ backgroundColor: color.border }}
              />
              <span className="text-xs font-medium text-white">{event.type}</span>
            </div>
            <div className="mt-0.5 text-[10px] text-slate-400 truncate max-w-[200px]">
              {event.summary || event.createdAt}
            </div>
          </div>
        ),
      },
      style: {
        background: color.bg,
        border: `1.5px solid ${color.border}`,
        borderRadius: 10,
        padding: '8px 12px',
        width: 240,
      },
    });

    // Session → first event
    if (i === 0) {
      edges.push({
        id: `e-session-first`,
        source: sessionNodeId,
        target: nodeId,
        type: 'smoothstep',
        style: { stroke: '#0ea5e9', strokeWidth: 2 },
      });
    }

    // Sequential event chain
    if (prevEventNodeId) {
      edges.push({
        id: `e-chain-${i}`,
        source: prevEventNodeId,
        target: nodeId,
        type: 'smoothstep',
        animated: false,
        style: { stroke: '#334155', strokeWidth: 1.5 },
      });
    }
    prevEventNodeId = nodeId;

    // Event → Agent (if applicable)
    if (event.agentId && agentNodeIds.has(event.agentId)) {
      edges.push({
        id: `e-agent-${event.id}`,
        source: nodeId,
        target: agentNodeIds.get(event.agentId)!,
        type: 'smoothstep',
        style: { stroke: '#8b5cf6', strokeWidth: 1.5, strokeDasharray: '4 3' },
      });
    }
  });

  return { nodes, edges };
}

/* ── Main component ── */
export function SessionFlowGraph({
  sessionId,
  sessionTitle,
  events,
}: {
  sessionId: string;
  sessionTitle: string;
  events: EventSummary[];
}) {
  const { nodes: initNodes, edges: initEdges } = useMemo(
    () => buildSessionGraph(sessionId, sessionTitle, events),
    [sessionId, sessionTitle, events],
  );

  const [nodes, , onNodesChange] = useNodesState(initNodes);
  const [flowEdges, , onEdgesChange] = useEdgesState(initEdges);
  const [selectedNode, setSelectedNode] = useState<Node | null>(null);

  const onNodeClick = useCallback((_: React.MouseEvent, node: Node) => {
    setSelectedNode((prev) => (prev?.id === node.id ? null : node));
  }, []);

  const onPaneClick = useCallback(() => setSelectedNode(null), []);

  if (events.length === 0) {
    return (
      <div className="rounded-2xl border border-dashed border-slate-700 px-4 py-10 text-center text-sm text-slate-400">
        No graphable events yet for this session.
      </div>
    );
  }

  // Count event types for the summary
  const typeCounts = new Map<string, number>();
  events.forEach((e) => typeCounts.set(e.type, (typeCounts.get(e.type) ?? 0) + 1));

  return (
    <div className="h-[500px] rounded-2xl border border-slate-800 bg-slate-950/70 overflow-hidden">
      <ReactFlow
        nodes={nodes}
        edges={flowEdges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={onNodeClick}
        onPaneClick={onPaneClick}
        colorMode={'dark' as ColorMode}
        fitView
        fitViewOptions={{ padding: 0.2, maxZoom: 1 }}
        proOptions={{ hideAttribution: true }}
        minZoom={0.15}
        maxZoom={2}
      >
        <Background color="#1e293b" gap={20} size={1} />
        <Controls
          className="!bg-slate-900 !border-slate-700 !rounded-xl !shadow-xl [&>button]:!bg-slate-800 [&>button]:!border-slate-700 [&>button]:!text-slate-300 [&>button:hover]:!bg-slate-700"
        />
        <MiniMap
          className="!bg-slate-900/80 !border-slate-700 !rounded-xl"
          maskColor="rgba(2, 6, 23, 0.7)"
        />

        <Panel position="top-left" className="!m-2">
          <div className="rounded-lg border border-slate-700 bg-slate-900/90 px-3 py-2 backdrop-blur">
            <p className="text-xs text-slate-400">
              {events.length} events · {new Set(events.map((e) => e.agentId).filter(Boolean)).size} agents
            </p>
            <div className="mt-1 flex flex-wrap gap-1.5">
              {Array.from(typeCounts.entries())
                .sort((a, b) => b[1] - a[1])
                .slice(0, 6)
                .map(([type, count]) => {
                  const color = getEventColor(type);
                  return (
                    <span
                      key={type}
                      className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]"
                      style={{ backgroundColor: color.bg, border: `1px solid ${color.border}`, color: '#e2e8f0' }}
                    >
                      {type} ({count})
                    </span>
                  );
                })}
            </div>
          </div>
        </Panel>
      </ReactFlow>

      {/* Selected node detail popover */}
      {selectedNode && (
        <div className="absolute bottom-3 left-3 z-10 max-w-xs rounded-xl border border-slate-700 bg-slate-900/95 p-3 shadow-xl backdrop-blur">
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium text-slate-300">Node detail</span>
            <button
              type="button"
              onClick={() => setSelectedNode(null)}
              className="text-slate-500 hover:text-white"
              aria-label="Close"
            >
              ×
            </button>
          </div>
          <p className="mt-1 text-xs text-slate-400 break-all">{selectedNode.id}</p>
        </div>
      )}
    </div>
  );
}
