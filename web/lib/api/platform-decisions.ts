import { apiFetch, tryApiFetch, getWebDataMode } from '@/lib/api/client';
import type { DecisionListData, DecisionSummary, DecisionTraceData } from '@/lib/models/platform';

type ApiDecisionTraceResponse = {
  session_id: string;
  focus: string;
  overview: {
    total_events: number;
    total_decisions: number;
    duration_minutes: number | null;
    unique_skills_used: number;
    error_count: number;
    error_rate_pct: number;
    top_event_types: [string, number][];
    top_skills: [string, number][];
  };
  diagnoses: {
    category: string;
    severity: string;
    summary: string;
    samples: string[];
    occurrences: number;
    affected_tool: string;
    fix_hint: string;
  }[];
  insights: {
    severity: string;
    category: string;
    message: string;
    evidence: string;
  }[];
  recommendations: string[];
};

function normalizeDecisionTrace(raw: ApiDecisionTraceResponse): DecisionTraceData {
  return {
    sessionId: raw.session_id,
    focus: raw.focus,
    overview: {
      totalEvents: raw.overview.total_events,
      totalDecisions: raw.overview.total_decisions,
      durationMinutes: raw.overview.duration_minutes,
      uniqueSkillsUsed: raw.overview.unique_skills_used,
      errorCount: raw.overview.error_count,
      errorRatePct: raw.overview.error_rate_pct,
      topEventTypes: raw.overview.top_event_types,
      topSkills: raw.overview.top_skills,
    },
    diagnoses: raw.diagnoses.map((d) => ({
      category: d.category,
      severity: d.severity as 'critical' | 'warning' | 'info',
      summary: d.summary,
      samples: d.samples,
      occurrences: d.occurrences,
      affectedTool: d.affected_tool,
      fixHint: d.fix_hint,
    })),
    insights: raw.insights.map((i) => ({
      severity: i.severity as 'critical' | 'warning' | 'info',
      category: i.category,
      message: i.message,
      evidence: i.evidence,
    })),
    recommendations: raw.recommendations,
  };
}

export async function getDecisionTrace(
  sessionId: string,
  opts?: { lastN?: number; question?: string },
): Promise<DecisionTraceData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const qs = new URLSearchParams();
    if (opts?.lastN) qs.set('last_n', String(opts.lastN));
    if (opts?.question) qs.set('question', opts.question);
    const suffix = qs.toString() ? `?${qs}` : '';
    const raw = await apiFetch<ApiDecisionTraceResponse>(
      `/chat/session/${sessionId}/decision-trace${suffix}`,
    );
    return normalizeDecisionTrace(raw);
  } catch {
    return null;
  }
}

// ── Decisions audit ─────────────────────────────────────────────────────────

type ApiDecisionAuditEntry = {
  id?: string;
  decision_id?: string;
  type?: string;
  decision_type?: string;
  status?: string;
  timestamp?: string;
  created_at?: string;
};

type ApiDecisionListResponse = {
  decisions: ApiDecisionAuditEntry[];
  total: number;
  limit: number;
  offset: number;
};

function normalizeDecisionAuditEntry(raw: ApiDecisionAuditEntry): DecisionSummary {
  return {
    id: raw.id ?? raw.decision_id ?? '',
    type: raw.type ?? raw.decision_type ?? 'unknown',
    status: raw.status ?? 'unknown',
    timestamp: raw.timestamp ?? raw.created_at ?? '',
  };
}

export async function getDecisions(
  limit = 50,
  offset = 0,
): Promise<DecisionListData> {
  const response = await tryApiFetch<ApiDecisionListResponse>(
    `/decisions?limit=${limit}&offset=${offset}`,
  );
  if (!response) {
    return { decisions: [], total: 0, limit, offset };
  }
  return {
    decisions: response.decisions.map(normalizeDecisionAuditEntry),
    total: response.total,
    limit: response.limit,
    offset: response.offset,
  };
}
