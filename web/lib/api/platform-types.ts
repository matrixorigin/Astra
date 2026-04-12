import type {
  AgentSummary,
  EventSummary,
  HealthSummary,
  OverviewData,
  SessionSummary,
} from '@/lib/models/platform';

export type ApiHealthResponse = {
  status: string;
  database: string;
  persist_ok: number;
  persist_fail: number;
};

export type ApiAgent = {
  agent_id: string;
  name: string;
  agent_type: string;
  owner_user_id: string;
  agent_config: Record<string, unknown>;
  is_active: boolean;
  updated_at?: string;
};

export type ApiAgentListResponse = {
  agents: ApiAgent[];
  total: number;
};

export type ApiSession = {
  session_id: string;
  user_id: string;
  agent_id?: string;
  title?: string;
  status: string;
  event_count: number;
  created_at: string;
  updated_at?: string;
};

export type ApiSessionListResponse = {
  sessions: ApiSession[];
  total: number;
  limit: number;
  offset: number;
};

export type ApiEvent = {
  event_id: string;
  session_id: string;
  event_type: string;
  content: string;
  agent_id?: string;
  created_at: string;
};

export type ApiEventListResponse = {
  events: ApiEvent[];
  total: number;
  limit: number;
  offset: number;
};

export type ApiReflectResponse = {
  report?: string;
  summary?: string;
  diagnosis?: string;
};

export type ApiPlatformSnapshot = {
  health: ApiHealthResponse;
  agents: ApiAgentListResponse;
  sessions: ApiSessionListResponse;
  events: ApiEventListResponse;
  timestamp: string;
};

export type ApiRunStatus = {
  run_id: string;
  session_id: string;
  status: string;
  waiting_for?: string;
  events_count: number;
};

export type ApiRunListResponse = {
  runs: ApiRunStatus[];
  total: number;
  limit: number;
  offset: number;
};

export type ApiSessionActivityEntry = {
  log_id: string;
  action: string;
  details: unknown;
  created_at: string;
};

export type ApiSessionActivityResponse = {
  session_id: string;
  activities: ApiSessionActivityEntry[];
  total: number;
};

export function readStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }

  return value.filter((item): item is string => typeof item === 'string');
}

export function readModelName(config: Record<string, unknown>): string {
  const directModel = config.model;
  if (typeof directModel === 'string' && directModel.length > 0) {
    return directModel;
  }

  const modelName = config.model_name;
  if (typeof modelName === 'string' && modelName.length > 0) {
    return modelName;
  }

  return 'unassigned';
}

export function normalizeAgent(agent: ApiAgent): AgentSummary {
  const skills =
    readStringArray(agent.agent_config.skill_filter) ||
    readStringArray(agent.agent_config.skills);

  return {
    id: agent.agent_id,
    name: agent.name,
    type: agent.agent_type,
    owner: agent.owner_user_id,
    status: agent.is_active ? 'active' : 'inactive',
    model: readModelName(agent.agent_config),
    skills,
    updatedAt: agent.updated_at,
  };
}

export function normalizeSession(session: ApiSession): SessionSummary {
  return {
    id: session.session_id,
    title: session.title ?? 'Untitled session',
    owner: session.user_id,
    status: session.status,
    agentId: session.agent_id,
    eventCount: session.event_count,
    createdAt: session.created_at,
    updatedAt: session.updated_at,
  };
}

function parseEventStatus(content: string): string | undefined {
  try {
    const parsed = JSON.parse(content) as unknown;
    if (!parsed || typeof parsed !== 'object') {
      return undefined;
    }

    const record = parsed as Record<string, unknown>;
    if (typeof record.status === 'string' && record.status.length > 0) {
      return record.status;
    }
    if (record.cancelled === true) {
      return 'cancelled';
    }

    const data = record.data;
    if (!data || typeof data !== 'object') {
      return undefined;
    }

    const dataRecord = data as Record<string, unknown>;
    if (typeof dataRecord.status === 'string' && dataRecord.status.length > 0) {
      return dataRecord.status;
    }
    if (dataRecord.cancelled === true) {
      return 'cancelled';
    }
  } catch {
    return undefined;
  }

  return undefined;
}

export function normalizeEvent(event: ApiEvent): EventSummary {
  return {
    id: event.event_id,
    sessionId: event.session_id,
    type: event.event_type,
    summary: event.content,
    agentId: event.agent_id,
    status: parseEventStatus(event.content),
    createdAt: event.created_at,
  };
}

export function normalizeHealth(health: ApiHealthResponse): HealthSummary {
  return {
    status: health.status,
    database: health.database,
    persistOk: health.persist_ok,
    persistFail: health.persist_fail,
  };
}

export function buildOverviewData(
  health: HealthSummary,
  agents: AgentSummary[],
  sessions: SessionSummary[],
  events: EventSummary[],
): OverviewData {
  return {
    health,
    stats: {
      activeAgents: agents.filter((agent) => agent.status === 'active').length,
      openSessions: sessions.filter((session) => session.status !== 'closed').length,
      recentEvents: events.length,
      persistOk: health.persistOk,
    },
    agents,
    sessions,
    events,
  };
}
