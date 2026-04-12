import {
  buildPlanGraphData,
  buildPlanGraphFromPlan,
  demoPlanGraphData,
} from '@/lib/graph/data';

describe('graph/data — normalizeTask', () => {
  it('normalizes snake_case task fields to camelCase', () => {
    const raw = {
      task_id: 't1',
      title: 'Test task',
      session_id: 's1',
      parent_task_id: 'p1',
      status: 'in_progress',
      progress_pct: 50,
      items_done: 2,
      items_total: 4,
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-02T00:00:00Z',
      completed_at: null,
      plan: {
        subtasks: [
          {
            id: 'sub1',
            title: 'Subtask 1',
            depends_on: [],
            status: 'completed',
            effort: 'small',
            files: ['file1.rs'],
          },
          {
            id: 'sub2',
            title: 'Subtask 2',
            depends_on: ['sub1'],
            status: 'pending',
            effort: 'medium',
            files: [],
          },
        ],
        notes: 'Some notes',
      },
    };

    const result = buildPlanGraphFromPlan(raw);

    expect(result.task.taskId).toBe('t1');
    expect(result.task.sessionId).toBe('s1');
    expect(result.task.parentTaskId).toBe('p1');
    expect(result.task.status).toBe('in_progress');
    expect(result.task.progressPct).toBe(50);
    expect(result.task.itemsDone).toBe(2);
    expect(result.task.itemsTotal).toBe(4);
    expect(result.task.plan?.subtasks).toHaveLength(2);
    expect(result.task.plan?.subtasks[0].dependsOn).toEqual([]);
    expect(result.task.plan?.subtasks[1].dependsOn).toEqual(['sub1']);
    expect(result.task.plan?.notes).toBe('Some notes');
    expect(result.delegations).toEqual([]);
    expect(result.progressEvents).toEqual([]);
  });

  it('handles camelCase input (already normalized)', () => {
    const raw = {
      taskId: 't2',
      title: 'Already camelCase',
      status: 'completed',
      progressPct: 100,
      itemsDone: 5,
      itemsTotal: 5,
      createdAt: '2026-01-01T00:00:00Z',
      updatedAt: '2026-01-01T00:00:00Z',
    };

    const result = buildPlanGraphFromPlan(raw);
    expect(result.task.taskId).toBe('t2');
    expect(result.task.status).toBe('completed');
    expect(result.task.progressPct).toBe(100);
  });

  it('normalizes status aliases', () => {
    const cases: Array<[string, string]> = [
      ['done', 'completed'],
      ['canceled', 'cancelled'],
      ['inprogress', 'in_progress'],
      ['PENDING', 'pending'],
      ['unknown_status', 'pending'], // fallback
    ];

    for (const [input, expected] of cases) {
      const result = buildPlanGraphFromPlan({
        task_id: 'x',
        title: 'x',
        status: input,
        created_at: '',
        updated_at: '',
      });
      expect(result.task.status).toBe(expected);
    }
  });
});

describe('graph/data — extractProgressEvents', () => {
  it('extracts PlanProgress events from raw events', () => {
    const taskRaw = {
      task_id: 't1',
      title: 'Test',
      status: 'in_progress',
      created_at: '',
      updated_at: '',
    };
    const eventsRaw = [
      {
        event_type: 'PlanProgress',
        ts: '2026-01-01T00:00:00Z',
        metadata: {
          subtask_id: 'sub1',
          subtask_title: 'Design',
          action: 'completed',
          progress_pct: 25,
          total_subtasks: 4,
          completed_subtasks: 1,
        },
      },
      {
        event_type: 'Turn', // not a progress event
        ts: '2026-01-01T00:01:00Z',
      },
      {
        event_type: 'plan_progress', // lowercase variant
        ts: '2026-01-01T00:02:00Z',
        metadata: {
          subtask_id: 'sub2',
          subtask_title: 'Build',
          action: 'started',
          progress_pct: 25,
          total_subtasks: 4,
          completed_subtasks: 1,
        },
      },
    ];

    const result = buildPlanGraphData(taskRaw, eventsRaw);
    expect(result.progressEvents).toHaveLength(2);
    expect(result.progressEvents[0].subtaskId).toBe('sub1');
    expect(result.progressEvents[0].action).toBe('completed');
    expect(result.progressEvents[1].subtaskId).toBe('sub2');
  });
});

describe('graph/data — extractDelegations', () => {
  it('tracks agent delegation lifecycle', () => {
    const taskRaw = {
      task_id: 't1',
      title: 'Test',
      status: 'in_progress',
      created_at: '',
      updated_at: '',
    };
    const eventsRaw = [
      {
        event_type: 'agent_delegated',
        ts: '2026-01-01T00:00:00Z',
        metadata: { agent_id: 'code-agent', from_agent_id: 'orchestrator', task: 'Build it' },
      },
      {
        event_type: 'agent_progress',
        ts: '2026-01-01T00:01:00Z',
        metadata: { agent_id: 'code-agent' },
      },
      {
        event_type: 'agent_completed',
        ts: '2026-01-01T00:02:00Z',
        metadata: { agent_id: 'code-agent' },
      },
    ];

    const result = buildPlanGraphData(taskRaw, eventsRaw);
    expect(result.delegations).toHaveLength(1);
    expect(result.delegations[0].fromAgentId).toBe('orchestrator');
    expect(result.delegations[0].toAgentId).toBe('code-agent');
    expect(result.delegations[0].status).toBe('completed');
  });

  it('preserves failed and cancelled agent completion states', () => {
    const taskRaw = {
      task_id: 't1',
      title: 'Test',
      status: 'in_progress',
      created_at: '',
      updated_at: '',
    };
    const eventsRaw = [
      {
        event_type: 'agent_delegated',
        ts: '2026-01-01T00:00:00Z',
        metadata: { agent_id: 'failed-agent', task: 'Do risky thing' },
      },
      {
        type: 'agent_completed',
        timestamp: '2026-01-01T00:01:00Z',
        agent_id: 'failed-agent',
        status: 'failed',
      },
      {
        event_type: 'agent_delegated',
        ts: '2026-01-01T00:02:00Z',
        metadata: { agent_id: 'cancelled-agent', task: 'Do other thing' },
      },
      {
        type: 'agent_completed',
        timestamp: '2026-01-01T00:03:00Z',
        agent_id: 'cancelled-agent',
        status: 'cancelled',
      },
    ];

    const result = buildPlanGraphData(taskRaw, eventsRaw);
    expect(result.delegations).toHaveLength(2);
    expect(result.delegations[0].status).toBe('failed');
    expect(result.delegations[1].status).toBe('cancelled');
  });
});

describe('graph/data — demoPlanGraphData', () => {
  it('returns well-formed demo data', () => {
    const data = demoPlanGraphData();

    expect(data.task.taskId).toBeTruthy();
    expect(data.task.title).toBeTruthy();
    expect(data.task.plan?.subtasks.length).toBeGreaterThan(0);
    expect(data.delegations.length).toBeGreaterThan(0);
    expect(data.progressEvents.length).toBeGreaterThan(0);

    // Check subtask dependency graph is valid
    const ids = new Set(data.task.plan!.subtasks.map((s) => s.id));
    for (const s of data.task.plan!.subtasks) {
      for (const dep of s.dependsOn) {
        expect(ids.has(dep)).toBe(true);
      }
    }
  });
});
