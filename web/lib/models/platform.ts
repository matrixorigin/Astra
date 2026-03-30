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
