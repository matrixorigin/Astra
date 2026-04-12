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
  status?: string;
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

// ── Evaluation types ──

export type QualityTrendPoint = {
  date: string;
  avgScore: number;
  count: number;
  model: string;
};

export type QualityTrendData = {
  points: QualityTrendPoint[];
  overallAvg: number;
  totalEvents: number;
};

export type DriftSignal = {
  model: string;
  templateId: string;
  currentAvg: number;
  previousAvg: number;
  delta: number;
  severity: 'critical' | 'warning' | 'info';
  sampleCount: number;
};

export type DriftData = {
  signals: DriftSignal[];
  checkedAt: string;
};

export type TrustReportData = {
  agentId: string;
  periodDays: number;
  totalChecks: number;
  safeCount: number;
  trustRatio: number;
  hallucinationRate: number;
};

export type SloAgent = {
  agentId: string;
  sloName: string;
  target: number;
  actual: number;
  met: boolean;
};

export type SloDashboardData = {
  periodDays: number;
  agents: SloAgent[];
};

export type MemoryHealthData = {
  totalMemories: number;
  activeMemories: number;
  inactiveMemories: number;
  staleWorkingMemories: number;
  orphanedRecords: number;
  healthy: boolean;
};

export type MemoryMetricsData = {
  totalMemories: number;
  avgConfidence: number;
  staleCount: number;
};

export type ObservabilityMetricsData = {
  agentId: string;
  periodDays: number;
  decision: { avgQuality: number; totalDecisions: number };
  session: { uniqueSessions: number; avgTurnsPerSession: number };
  skill: { totalInvocations: number; successCount: number; successRate: number };
};

// ── Model types ──

export type ModelPricing = {
  prompt: number;
  completion: number;
};

export type ModelQuirks = {
  preserveReasoningContent: boolean;
  noParallelToolCalls: boolean;
  toolChoiceRequired: boolean;
  strictToolCallIds: boolean;
  noSystemMessage: boolean;
  systemAsUserPrefix: boolean;
};

export type ModelSummary = {
  modelId: string;
  name: string;
  provider: string;
  baseUrl: string;
  description: string | null;
  isActive: boolean;
  contextWindow: number | null;
  maxCompletionTokens: number | null;
  inputModalities: string[];
  outputModalities: string[];
  supportedParameters: string[];
  pricing: ModelPricing;
  architecture: string | null;
  tags: string[];
  quirks: ModelQuirks;
};

export type ModelDetail = ModelSummary;

// ── Decision audit types ──

export type DecisionSummary = {
  id: string;
  type: string;
  status: string;
  timestamp: string;
};

export type DecisionListData = {
  decisions: DecisionSummary[];
  total: number;
  limit: number;
  offset: number;
};
