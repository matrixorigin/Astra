import type { Node, Edge } from '@xyflow/react';
import type { SubtaskPlan, DelegationEvent, PlanGraphData } from './types';

/**
 * Convert PlanGraphData into React Flow nodes and edges with an
 * automatic layered (Sugiyama-style) layout based on dependency depth.
 */

// ── Node dimensions ──────────
const NODE_W = 260;
const NODE_H = 80;
const GAP_X = 60;
const GAP_Y = 100;
const DELEGATION_OFFSET_Y = 80;

// ── Color palette by status ──────────
export const statusColors: Record<string, { bg: string; border: string; text: string }> = {
  pending:     { bg: '#0f172a', border: '#334155', text: '#94a3b8' },
  in_progress: { bg: '#0c4a6e', border: '#0ea5e9', text: '#e0f2fe' },
  paused:      { bg: '#422006', border: '#f59e0b', text: '#fef3c7' },
  completed:   { bg: '#052e16', border: '#22c55e', text: '#dcfce7' },
  failed:      { bg: '#450a0a', border: '#ef4444', text: '#fee2e2' },
  cancelled:   { bg: '#1e1b4b', border: '#6366f1', text: '#e0e7ff' },
};

export const effortBadge: Record<string, string> = {
  small:  '🟢 S',
  medium: '🟡 M',
  large:  '🔴 L',
};

// ── Layout computation ──────────

/** Compute the depth (layer) of each subtask via topological sort */
function computeLayers(subtasks: SubtaskPlan[]): Map<string, number> {
  const depths = new Map<string, number>();
  const byId = new Map(subtasks.map((s) => [s.id, s]));

  function depth(id: string): number {
    if (depths.has(id)) return depths.get(id)!;
    const node = byId.get(id);
    if (!node || node.dependsOn.length === 0) {
      depths.set(id, 0);
      return 0;
    }
    const d = 1 + Math.max(...node.dependsOn.map((dep) => depth(dep)));
    depths.set(id, d);
    return d;
  }

  subtasks.forEach((s) => depth(s.id));
  return depths;
}

/** Group subtask IDs by their layer */
function groupByLayer(layers: Map<string, number>): string[][] {
  const maxLayer = Math.max(...layers.values(), 0);
  const groups: string[][] = Array.from({ length: maxLayer + 1 }, () => []);
  layers.forEach((layer, id) => groups[layer].push(id));
  return groups;
}

// ── Build nodes & edges ──────────

export function buildFlowElements(data: PlanGraphData): {
  nodes: Node[];
  edges: Edge[];
} {
  const nodes: Node[] = [];
  const edges: Edge[] = [];
  const subtasks = data.task.plan?.subtasks ?? [];

  if (subtasks.length === 0) {
    // Single "no plan" placeholder
    nodes.push({
      id: 'empty',
      type: 'subtaskNode',
      position: { x: 0, y: 0 },
      data: {
        label: data.task.title || 'No plan available',
        status: data.task.status,
        effort: undefined,
        filesCount: 0,
        description: 'This task has no decomposed plan yet.',
      },
    });
    return { nodes, edges };
  }

  // Layout subtask nodes
  const layers = computeLayers(subtasks);
  const groups = groupByLayer(layers);
  const byId = new Map(subtasks.map((s) => [s.id, s]));

  groups.forEach((ids, layerIdx) => {
    const totalWidth = ids.length * NODE_W + (ids.length - 1) * GAP_X;
    const startX = -totalWidth / 2;

    ids.forEach((id, colIdx) => {
      const s = byId.get(id)!;
      nodes.push({
        id: s.id,
        type: 'subtaskNode',
        position: {
          x: startX + colIdx * (NODE_W + GAP_X),
          y: layerIdx * (NODE_H + GAP_Y),
        },
        data: {
          label: s.title,
          status: s.status,
          effort: s.effort,
          filesCount: s.files.length,
          description: s.description,
          acceptance: s.acceptance,
        },
      });

      // Dependency edges
      s.dependsOn.forEach((dep) => {
        if (byId.has(dep)) {
          edges.push({
            id: `e-${dep}-${s.id}`,
            source: dep,
            target: s.id,
            type: 'smoothstep',
            animated: s.status === 'in_progress',
            style: {
              stroke: s.status === 'in_progress' ? '#0ea5e9' : '#334155',
              strokeWidth: 2,
            },
          });
        }
      });
    });
  });

  // Delegation nodes (placed below the main graph)
  const maxLayer = Math.max(...layers.values(), 0);
  const delegationBaseY = (maxLayer + 1) * (NODE_H + GAP_Y) + DELEGATION_OFFSET_Y;

  data.delegations.forEach((del, i) => {
    const nodeId = `delegation-${del.id}`;
    nodes.push({
      id: nodeId,
      type: 'delegationNode',
      position: {
        x: i * (NODE_W + GAP_X) - (data.delegations.length * (NODE_W + GAP_X)) / 2,
        y: delegationBaseY,
      },
      data: {
        label: del.taskDescription || `→ ${del.toAgentId}`,
        fromAgent: del.fromAgentId,
        toAgent: del.toAgentId,
        status: del.status,
      },
    });

    // Edge from closest related subtask (heuristic: match by description keywords)
    const relatedSubtask = findRelatedSubtask(subtasks, del.taskDescription);
    if (relatedSubtask) {
      edges.push({
        id: `e-del-${del.id}`,
        source: relatedSubtask.id,
        target: nodeId,
        type: 'smoothstep',
        animated: del.status === 'delegated' || del.status === 'in_progress',
        style: {
          stroke: '#8b5cf6',
          strokeWidth: 2,
          strokeDasharray: '6 3',
        },
      });
    }
  });

  return { nodes, edges };
}

/** Heuristic: find the subtask most related to a delegation task description */
function findRelatedSubtask(
  subtasks: SubtaskPlan[],
  taskDescription: string,
): SubtaskPlan | undefined {
  if (!taskDescription) return undefined;
  const words = taskDescription.toLowerCase().split(/\s+/);
  let best: SubtaskPlan | undefined;
  let bestScore = 0;

  for (const s of subtasks) {
    const hay = `${s.title} ${s.description ?? ''}`.toLowerCase();
    const score = words.filter((w) => w.length > 3 && hay.includes(w)).length;
    if (score > bestScore) {
      bestScore = score;
      best = s;
    }
  }

  return bestScore >= 1 ? best : undefined;
}
