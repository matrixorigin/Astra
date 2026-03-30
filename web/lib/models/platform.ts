export type AgentSummary = {
  id: string;
  name: string;
  type: string;
  owner: string;
  status: 'active' | 'inactive';
  model: string;
  skills: string[];
  updatedAt?: string;
};

export type SessionSummary = {
  id: string;
  title: string;
  owner: string;
  status: string;
  agentId?: string;
  eventCount: number;
  createdAt: string;
  updatedAt?: string;
};

export type EventSummary = {
  id: string;
  sessionId: string;
  type: string;
  summary: string;
  agentId?: string;
  createdAt: string;
};

export type HealthSummary = {
  status: string;
  database: string;
  persistOk: number;
  persistFail: number;
};

export type OverviewStats = {
  activeAgents: number;
  openSessions: number;
  recentEvents: number;
  persistOk: number;
};

export type OverviewData = {
  health: HealthSummary;
  stats: OverviewStats;
  agents: AgentSummary[];
  sessions: SessionSummary[];
  events: EventSummary[];
};

export type RunSummary = {
  runId: string;
  sessionId: string;
  status: string;
  waitingFor?: string;
  eventsCount: number;
};

export type RunListData = {
  runs: RunSummary[];
  total: number;
  limit: number;
  offset: number;
};

export type SessionActivityEntry = {
  logId: string;
  action: string;
  details: unknown;
  createdAt: string;
};

export type SessionActivityData = {
  sessionId: string;
  activities: SessionActivityEntry[];
  total: number;
};
