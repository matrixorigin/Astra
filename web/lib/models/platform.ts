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

// ── Decision trace types ──

export type SessionOverview = {
  totalEvents: number;
  totalDecisions: number;
  durationMinutes: number | null;
  uniqueSkillsUsed: number;
  errorCount: number;
  errorRatePct: number;
  topEventTypes: [string, number][];
  topSkills: [string, number][];
};

export type Diagnosis = {
  category: string;
  severity: 'critical' | 'warning' | 'info';
  summary: string;
  samples: string[];
  occurrences: number;
  affectedTool: string;
  fixHint: string;
};

export type Insight = {
  severity: 'critical' | 'warning' | 'info';
  category: string;
  message: string;
  evidence: string;
};

export type DecisionTraceData = {
  sessionId: string;
  focus: string;
  overview: SessionOverview;
  diagnoses: Diagnosis[];
  insights: Insight[];
  recommendations: string[];
};

// ── Introspection types ──

export type EpisodicStats = {
  turns: number;
  totalEvents: number;
  toolIntensity: string;
  sessionDepth: string;
};

export type SemanticStats = {
  ctxSnapshots: number;
  peakTokens: number;
  contextManagedTokens: number | null;
  lastAssemblyMs: number | null;
  llmPromptTokens: number | null;
  llmCompletionTokens: number | null;
  llmTotalTokens: number | null;
  health: Record<string, unknown> | null;
};

export type ProceduralStats = {
  skillSelections: number;
  accuracyRate: number | null;
};

export type MemoryIntrospectionData = {
  episodic: EpisodicStats;
  semantic: SemanticStats;
  procedural: ProceduralStats;
  profile: string[] | null;
};

export type SkillInfo = {
  name: string;
  version: string;
  description: string;
  category: string;
};

export type SkillsIntrospectionData = {
  installed: SkillInfo[];
  cloud: SkillInfo[];
};
