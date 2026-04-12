import type {
  TaskRecord,
  SubtaskPlan,
  DelegationEvent,
  PlanProgressEvent,
  PlanGraphData,
  TaskStatus,
} from './types';

// ── Normalize helpers (snake_case API → camelCase frontend) ──────────

function normalizeSubtask(raw: Record<string, unknown>): SubtaskPlan {
  return {
    id: (raw.id as string) ?? '',
    title: (raw.title as string) ?? '',
    description: (raw.description as string) ?? undefined,
    dependsOn: (raw.depends_on as string[]) ?? (raw.dependsOn as string[]) ?? [],
    status: normalizeStatus(raw.status as string),
    effort: (raw.effort as SubtaskPlan['effort']) ?? undefined,
    files: (raw.files as string[]) ?? [],
    acceptance: (raw.acceptance as string) ?? undefined,
  };
}

function normalizeStatus(s: string | undefined): TaskStatus {
  if (!s) return 'pending';
  const mapped: Record<string, TaskStatus> = {
    pending: 'pending',
    in_progress: 'in_progress',
    inprogress: 'in_progress',
    paused: 'paused',
    completed: 'completed',
    done: 'completed',
    failed: 'failed',
    cancelled: 'cancelled',
    canceled: 'cancelled',
  };
  return mapped[s.toLowerCase()] ?? 'pending';
}

function normalizeTask(raw: Record<string, unknown>): TaskRecord {
  const planRaw = (raw.plan ?? raw.plan_json) as Record<string, unknown> | undefined;
  let plan = undefined;
  if (planRaw && typeof planRaw === 'object') {
    const subtasksRaw = (planRaw.subtasks as Record<string, unknown>[]) ?? [];
    plan = {
      subtasks: subtasksRaw.map(normalizeSubtask),
      notes: (planRaw.notes as string) ?? undefined,
    };
  }

  return {
    taskId: (raw.task_id as string) ?? (raw.taskId as string) ?? '',
    title: (raw.title as string) ?? '',
    sessionId: (raw.session_id as string) ?? (raw.sessionId as string) ?? undefined,
    parentTaskId:
      (raw.parent_task_id as string) ?? (raw.parentTaskId as string) ?? undefined,
    plan,
    status: normalizeStatus(raw.status as string),
    progressPct: (raw.progress_pct as number) ?? (raw.progressPct as number) ?? 0,
    itemsDone: (raw.items_done as number) ?? (raw.itemsDone as number) ?? 0,
    itemsTotal: (raw.items_total as number) ?? (raw.itemsTotal as number) ?? 0,
    createdAt: (raw.created_at as string) ?? (raw.createdAt as string) ?? '',
    updatedAt: (raw.updated_at as string) ?? (raw.updatedAt as string) ?? '',
    completedAt:
      (raw.completed_at as string) ?? (raw.completedAt as string) ?? undefined,
  };
}

// ── Extract plan progress & delegation from raw events ──────────

function extractProgressEvents(
  events: Record<string, unknown>[],
): PlanProgressEvent[] {
  return events
    .filter(
      (e) =>
        (e.event_type ?? e.eventType ?? e.type) === 'PlanProgress' ||
        (e.event_type ?? e.eventType ?? e.type) === 'plan_progress',
    )
    .map((e) => {
      const meta = (e.metadata ?? e.meta ?? {}) as Record<string, unknown>;
      return {
        subtaskId: (meta.subtask_id as string) ?? (meta.subtaskId as string) ?? '',
        subtaskTitle:
          (meta.subtask_title as string) ?? (meta.subtaskTitle as string) ?? '',
        action: (meta.action as PlanProgressEvent['action']) ?? 'started',
        progressPct:
          (meta.progress_pct as number) ?? (meta.progressPct as number) ?? 0,
        totalSubtasks:
          (meta.total_subtasks as number) ?? (meta.totalSubtasks as number) ?? 0,
        completedSubtasks:
          (meta.completed_subtasks as number) ??
          (meta.completedSubtasks as number) ??
          0,
        timestamp: (e.ts as string) ?? (e.timestamp as string) ?? '',
      };
    });
}

function extractDelegations(
  events: Record<string, unknown>[],
): DelegationEvent[] {
  const delegations: DelegationEvent[] = [];
  const delegationMap = new Map<string, DelegationEvent>();

  for (const e of events) {
    const type = (e.event_type ?? e.eventType ?? e.type) as string;
    const meta = (e.metadata ?? e.meta ?? e.data ?? {}) as Record<string, unknown>;
    const agentId =
      (meta.agent_id as string) ??
      (meta.agentId as string) ??
      (e.agent_id as string) ??
      (e.agentId as string) ??
      '';
    const fromAgent =
      (meta.from_agent_id as string) ??
      (meta.fromAgentId as string) ??
      (e.agent_id as string) ??
      'main';

    if (type === 'agent_delegated') {
      const d: DelegationEvent = {
        id: `del-${agentId}-${delegations.length}`,
        fromAgentId: fromAgent,
        toAgentId: agentId,
        taskDescription: (meta.task as string) ?? (meta.description as string) ?? '',
        status: 'delegated',
        timestamp: (e.ts as string) ?? (e.timestamp as string) ?? '',
      };
      delegationMap.set(agentId, d);
      delegations.push(d);
    } else if (type === 'agent_progress' && delegationMap.has(agentId)) {
      delegationMap.get(agentId)!.status = 'in_progress';
    } else if (type === 'agent_completed' && delegationMap.has(agentId)) {
      const completionStatus = normalizeStatus(
        ((e.status as string | undefined) ?? (meta.status as string | undefined)) as
          | string
          | undefined,
      );
      delegationMap.get(agentId)!.status =
        completionStatus === 'failed' || completionStatus === 'cancelled'
          ? completionStatus
          : 'completed';
    }
  }

  return delegations;
}

// ── Public API ──────────

/**
 * Build PlanGraphData from raw API responses.
 * `taskRaw` comes from a task endpoint, `eventsRaw` from event stream.
 */
export function buildPlanGraphData(
  taskRaw: Record<string, unknown>,
  eventsRaw: Record<string, unknown>[],
): PlanGraphData {
  return {
    task: normalizeTask(taskRaw),
    delegations: extractDelegations(eventsRaw),
    progressEvents: extractProgressEvents(eventsRaw),
  };
}

/** Build graph data from just a plan (when no events available) */
export function buildPlanGraphFromPlan(
  taskRaw: Record<string, unknown>,
): PlanGraphData {
  return {
    task: normalizeTask(taskRaw),
    delegations: [],
    progressEvents: [],
  };
}

// ── Demo data for when backend is unavailable ──────────

export function demoPlanGraphData(): PlanGraphData {
  return {
    task: {
      taskId: 'demo-task-001',
      title: 'Build authentication system',
      sessionId: 'session-abc',
      status: 'in_progress',
      progressPct: 60,
      itemsDone: 3,
      itemsTotal: 5,
      createdAt: new Date(Date.now() - 3600_000).toISOString(),
      updatedAt: new Date().toISOString(),
      plan: {
        subtasks: [
          {
            id: 'design',
            title: 'Design auth schema',
            description: 'Define JWT token format, user model, and permission scopes',
            dependsOn: [],
            status: 'completed',
            effort: 'small',
            files: ['docs/auth-design.md'],
          },
          {
            id: 'user-model',
            title: 'Implement user model',
            description: 'Create User struct with bcrypt password hashing',
            dependsOn: ['design'],
            status: 'completed',
            effort: 'medium',
            files: ['src/models/user.rs', 'src/storage/users.rs'],
          },
          {
            id: 'jwt-service',
            title: 'JWT token service',
            description: 'Sign / verify / refresh logic with HS256',
            dependsOn: ['design'],
            status: 'completed',
            effort: 'medium',
            files: ['src/auth/jwt.rs'],
          },
          {
            id: 'api-routes',
            title: 'Auth API endpoints',
            description: 'Login, register, refresh, logout, me',
            dependsOn: ['user-model', 'jwt-service'],
            status: 'in_progress',
            effort: 'large',
            files: ['src/server/auth_handlers.rs', 'src/server/router.rs'],
          },
          {
            id: 'tests',
            title: 'Integration tests',
            description: 'Test all auth flows end-to-end',
            dependsOn: ['api-routes'],
            status: 'pending',
            effort: 'medium',
            files: ['tests/auth_integration.rs'],
            acceptance: 'All auth endpoints return correct status codes and tokens',
          },
        ],
        notes: 'Using HS256 for simplicity. Can upgrade to RS256 later.',
      },
    },
    delegations: [
      {
        id: 'del-1',
        fromAgentId: 'orchestrator',
        toAgentId: 'code-agent',
        taskDescription: 'Implement JWT token service',
        status: 'completed',
        timestamp: new Date(Date.now() - 1800_000).toISOString(),
      },
      {
        id: 'del-2',
        fromAgentId: 'orchestrator',
        toAgentId: 'test-agent',
        taskDescription: 'Write auth integration tests',
        status: 'delegated',
        timestamp: new Date(Date.now() - 300_000).toISOString(),
      },
    ],
    progressEvents: [
      {
        subtaskId: 'design',
        subtaskTitle: 'Design auth schema',
        action: 'completed',
        progressPct: 20,
        totalSubtasks: 5,
        completedSubtasks: 1,
        timestamp: new Date(Date.now() - 3000_000).toISOString(),
      },
      {
        subtaskId: 'user-model',
        subtaskTitle: 'Implement user model',
        action: 'completed',
        progressPct: 40,
        totalSubtasks: 5,
        completedSubtasks: 2,
        timestamp: new Date(Date.now() - 2400_000).toISOString(),
      },
      {
        subtaskId: 'jwt-service',
        subtaskTitle: 'JWT token service',
        action: 'completed',
        progressPct: 60,
        totalSubtasks: 5,
        completedSubtasks: 3,
        timestamp: new Date(Date.now() - 1800_000).toISOString(),
      },
      {
        subtaskId: 'api-routes',
        subtaskTitle: 'Auth API endpoints',
        action: 'started',
        progressPct: 60,
        totalSubtasks: 5,
        completedSubtasks: 3,
        timestamp: new Date(Date.now() - 900_000).toISOString(),
      },
    ],
  };
}
