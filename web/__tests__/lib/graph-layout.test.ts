import { buildFlowElements, statusColors, effortBadge } from '@/lib/graph/layout';
import type { PlanGraphData } from '@/lib/graph/types';

describe('graph/layout — buildFlowElements', () => {
  const baseData: PlanGraphData = {
    task: {
      taskId: 't1',
      title: 'Test Plan',
      status: 'in_progress',
      progressPct: 50,
      itemsDone: 2,
      itemsTotal: 4,
      createdAt: '2026-01-01T00:00:00Z',
      updatedAt: '2026-01-01T00:00:00Z',
      plan: {
        subtasks: [
          { id: 'a', title: 'A', dependsOn: [], status: 'completed', files: [] },
          { id: 'b', title: 'B', dependsOn: ['a'], status: 'in_progress', files: [] },
          { id: 'c', title: 'C', dependsOn: ['a'], status: 'pending', files: [] },
          { id: 'd', title: 'D', dependsOn: ['b', 'c'], status: 'pending', files: [] },
        ],
      },
    },
    delegations: [],
    progressEvents: [],
  };

  it('creates nodes for every subtask', () => {
    const { nodes } = buildFlowElements(baseData);
    const subtaskNodes = nodes.filter((n) => n.type === 'subtaskNode');
    expect(subtaskNodes).toHaveLength(4);
    expect(subtaskNodes.map((n) => n.id).sort()).toEqual(['a', 'b', 'c', 'd']);
  });

  it('creates edges matching dependency graph', () => {
    const { edges } = buildFlowElements(baseData);
    const depEdges = edges.filter((e) => !e.id.startsWith('del-'));
    expect(depEdges).toHaveLength(4);
    expect(depEdges.find((e) => e.source === 'a' && e.target === 'b')).toBeDefined();
    expect(depEdges.find((e) => e.source === 'a' && e.target === 'c')).toBeDefined();
    expect(depEdges.find((e) => e.source === 'b' && e.target === 'd')).toBeDefined();
    expect(depEdges.find((e) => e.source === 'c' && e.target === 'd')).toBeDefined();
  });

  it('layers nodes by dependency depth (vertical layout)', () => {
    const { nodes } = buildFlowElements(baseData);
    const getY = (id: string) =>
      nodes.find((n) => n.id === id)!.position.y;

    // a at depth 0, b/c at depth 1, d at depth 2
    expect(getY('a')).toBeLessThan(getY('b'));
    expect(getY('a')).toBeLessThan(getY('c'));
    expect(getY('b')).toBeLessThan(getY('d'));
    expect(getY('c')).toBeLessThan(getY('d'));
    // b and c at the same layer (same y)
    expect(getY('b')).toBe(getY('c'));
  });

  it('includes delegation nodes and edges', () => {
    const data: PlanGraphData = {
      ...baseData,
      delegations: [
        {
          id: 'del-1',
          fromAgentId: 'main',
          toAgentId: 'helper',
          taskDescription: 'Help with B task',
          status: 'completed',
          timestamp: '2026-01-01T00:00:00Z',
        },
      ],
    };

    // Subtask titles are short ('A','B','C','D'). Add a subtask with matching description
    // so findRelatedSubtask can link the delegation.
    data.task.plan!.subtasks = [
      ...baseData.task.plan!.subtasks,
      { id: 'e', title: 'Helper task', dependsOn: ['a'], status: 'pending', files: [], description: 'Help with B task' },
    ];

    const { nodes, edges } = buildFlowElements(data);
    const delNodes = nodes.filter((n) => n.type === 'delegationNode');
    expect(delNodes).toHaveLength(1);

    const delEdges = edges.filter((e) => e.id.includes('del-'));
    expect(delEdges.length).toBeGreaterThan(0);
  });

  it('returns placeholder node for empty plan', () => {
    const data: PlanGraphData = {
      task: {
        taskId: 't2',
        title: 'Empty Task',
        status: 'pending',
        progressPct: 0,
        itemsDone: 0,
        itemsTotal: 0,
        createdAt: '',
        updatedAt: '',
      },
      delegations: [],
      progressEvents: [],
    };

    const { nodes, edges } = buildFlowElements(data);
    // Should have one placeholder node, no edges
    expect(nodes).toHaveLength(1);
    expect(nodes[0].id).toBe('empty');
    expect(edges).toHaveLength(0);
  });
});

describe('graph/layout — constants', () => {
  it('has colors for all expected statuses', () => {
    const expected = ['pending', 'in_progress', 'paused', 'completed', 'failed', 'cancelled'];
    for (const s of expected) {
      expect(statusColors[s]).toBeDefined();
      expect(statusColors[s].bg).toBeTruthy();
      expect(statusColors[s].border).toBeTruthy();
      expect(statusColors[s].text).toBeTruthy();
    }
  });

  it('has effort badges', () => {
    expect(effortBadge.small).toBeTruthy();
    expect(effortBadge.medium).toBeTruthy();
    expect(effortBadge.large).toBeTruthy();
  });
});
