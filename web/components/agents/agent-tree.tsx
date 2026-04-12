'use client';

import { useMemo } from 'react';
import type {
  AgentSpawnedEvent,
  AgentProgressEvent,
  AgentCompletedEvent,
  StreamEvent,
} from '@/lib/streaming/types';

// ─── Types ──────────────────────────────────────────────────────────────────

type AgentNode = {
  agentId: string;
  runId: string;
  parentRunId: string;
  agentType: string;
  description: string;
  status: 'running' | 'completed' | 'failed' | 'cancelled';
  children: AgentNode[];
  lastActivity?: string;
  timestamp?: number;
};

// ─── Status Icons ───────────────────────────────────────────────────────────

const STATUS_ICONS: Record<string, string> = {
  running: '▶',
  completed: '✓',
  failed: '✗',
  cancelled: '⊘',
};

const STATUS_COLORS: Record<string, string> = {
  running: 'text-blue-400',
  completed: 'text-emerald-400',
  failed: 'text-red-400',
  cancelled: 'text-slate-400',
};

// ─── Tree Builder ───────────────────────────────────────────────────────────

type AgentState = {
  agentId: string;
  runId: string;
  parentRunId: string;
  agentType: string;
  description: string;
  status: 'running' | 'completed' | 'failed' | 'cancelled';
  lastActivity?: string;
  timestamp?: number;
};

function buildTree(events: StreamEvent[]): AgentNode[] {
  const agents = new Map<string, AgentState>();

  for (const event of events) {
    switch (event.type) {
      case 'agent_spawned': {
        const e = event as AgentSpawnedEvent;
        agents.set(e.run_id, {
          agentId: e.agent_id,
          runId: e.run_id,
          parentRunId: e.parent_run_id,
          agentType: e.agent_type,
          description: e.description,
          status: 'running',
          timestamp: e.timestamp,
        });
        break;
      }
      case 'agent_progress': {
        const e = event as AgentProgressEvent;
        // Match by agent_id since we don't have run_id in progress events
        for (const agent of agents.values()) {
          if (agent.agentId === e.agent_id) {
            agent.lastActivity = e.status;
            break;
          }
        }
        break;
      }
      case 'agent_completed': {
        const e = event as AgentCompletedEvent;
        for (const agent of agents.values()) {
          if (agent.agentId === e.agent_id) {
            agent.status = e.status;
            break;
          }
        }
        break;
      }
    }
  }

  // Build tree from flat map
  const nodeMap = new Map<string, AgentNode>();
  for (const agent of agents.values()) {
    nodeMap.set(agent.runId, {
      ...agent,
      children: [],
    });
  }

  const roots: AgentNode[] = [];
  for (const node of nodeMap.values()) {
    const parent = nodeMap.get(node.parentRunId);
    if (parent) {
      parent.children.push(node);
    } else {
      roots.push(node);
    }
  }

  return roots;
}

// ─── Tree Node Component ────────────────────────────────────────────────────

function AgentTreeNode({ node, depth = 0 }: { node: AgentNode; depth?: number }) {
  const icon = STATUS_ICONS[node.status] ?? '?';
  const color = STATUS_COLORS[node.status] ?? 'text-slate-400';

  return (
    <div className={depth > 0 ? 'ml-4 border-l border-slate-700 pl-3' : ''}>
      <div className="flex items-center gap-2 py-1">
        <span className={`text-sm font-mono ${color}`}>{icon}</span>
        <span className="text-sm font-medium text-white">{node.agentId}</span>
        <span className="text-xs text-slate-500">({node.agentType})</span>
        {node.lastActivity && (
          <span className="text-xs text-slate-400">· {node.lastActivity}</span>
        )}
      </div>
      <p className="text-xs text-slate-400 ml-6 mb-1">{node.description}</p>
      {node.children.map((child) => (
        <AgentTreeNode key={child.runId} node={child} depth={depth + 1} />
      ))}
    </div>
  );
}

// ─── Main Component ─────────────────────────────────────────────────────────

export function AgentTree({ events }: { events: StreamEvent[] }) {
  const roots = useMemo(() => buildTree(events), [events]);

  if (roots.length === 0) {
    return null;
  }

  const totalAgents = events.filter((e) => e.type === 'agent_spawned').length;
  const completedAgents = events.filter(
    (e) => e.type === 'agent_completed' && e.status === 'completed',
  ).length;

  return (
    <div className="rounded-2xl border border-slate-800 bg-slate-950/70">
      <div className="flex items-center justify-between border-b border-slate-800 px-4 py-3">
        <div className="flex items-center gap-2">
          <span className="text-sm">🌲</span>
          <h3 className="text-sm font-medium text-white">Agent Tree</h3>
        </div>
        <span className="text-xs text-slate-500">
          {completedAgents}/{totalAgents} completed
        </span>
      </div>
      <div className="p-4 space-y-1">
        {roots.map((root) => (
          <AgentTreeNode key={root.runId} node={root} />
        ))}
      </div>
    </div>
  );
}
