import { apiFetch, getWebDataMode } from '@/lib/api/client';
import type {
  DriftData,
  MemoryHealthData,
  MemoryMetricsData,
  QualityTrendData,
  SloDashboardData,
  TrustReportData,
} from '@/lib/models/platform';

export async function getContextTrend(
  sessionId: string,
  opts?: { turns?: number; contextWindow?: number },
): Promise<Record<string, unknown> | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const qs = new URLSearchParams({ session_id: sessionId });
    if (opts?.turns) qs.set('turns', String(opts.turns));
    if (opts?.contextWindow) qs.set('context_window', String(opts.contextWindow));
    return await apiFetch<Record<string, unknown>>(
      `/introspection/context/trend?${qs}`,
    );
  } catch {
    return null;
  }
}

// ── Quality trend ───────────────────────────────────────────────────────────

type ApiQualityTrendResponse = {
  points: { date: string; avg_score: number; count: number; model: string }[];
  overall_avg: number;
  total_events: number;
};

export async function getQualityTrend(
  opts?: { days?: number; model?: string },
): Promise<QualityTrendData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const qs = new URLSearchParams();
    if (opts?.days) qs.set('days', String(opts.days));
    if (opts?.model) qs.set('model', opts.model);
    const suffix = qs.toString() ? `?${qs}` : '';
    const raw = await apiFetch<ApiQualityTrendResponse>(`/evaluation/quality/trend${suffix}`);
    return {
      points: raw.points.map((p) => ({
        date: p.date,
        avgScore: p.avg_score,
        count: p.count,
        model: p.model,
      })),
      overallAvg: raw.overall_avg,
      totalEvents: raw.total_events,
    };
  } catch {
    return null;
  }
}

// ── Drift signals ───────────────────────────────────────────────────────────

type ApiDriftResponse = {
  signals: {
    model: string;
    template_id: string;
    current_avg: number;
    previous_avg: number;
    delta: number;
    severity: string;
    sample_count: number;
  }[];
  checked_at: string;
};

export async function getDriftSignals(): Promise<DriftData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const raw = await apiFetch<ApiDriftResponse>('/evaluation/drift');
    return {
      signals: raw.signals.map((s) => ({
        model: s.model,
        templateId: s.template_id,
        currentAvg: s.current_avg,
        previousAvg: s.previous_avg,
        delta: s.delta,
        severity: s.severity as 'critical' | 'warning' | 'info',
        sampleCount: s.sample_count,
      })),
      checkedAt: raw.checked_at,
    };
  } catch {
    return null;
  }
}

// ── Trust report ────────────────────────────────────────────────────────────

type ApiTrustReportResponse = {
  agent_id: string;
  period_days: number;
  total_checks: number;
  safe_count: number;
  trust_ratio: number;
  hallucination_rate: number;
};

export async function getTrustReport(
  agentId: string,
  days?: number,
): Promise<TrustReportData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const qs = new URLSearchParams({ agent_id: agentId });
    if (days) qs.set('days', String(days));
    const raw = await apiFetch<ApiTrustReportResponse>(
      `/evaluation/trust-report?${qs}`,
    );
    return {
      agentId: raw.agent_id,
      periodDays: raw.period_days,
      totalChecks: raw.total_checks,
      safeCount: raw.safe_count,
      trustRatio: raw.trust_ratio,
      hallucinationRate: raw.hallucination_rate,
    };
  } catch {
    return null;
  }
}

// ── SLO dashboard ───────────────────────────────────────────────────────────

type ApiSloDashboardResponse = {
  period_days: number;
  agents: {
    agent_id: string;
    slo_name: string;
    target: number;
    actual: number;
    met: boolean;
  }[];
};

export async function getSloDashboard(
  periodDays?: number,
): Promise<SloDashboardData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const qs = periodDays ? `?period_days=${periodDays}` : '';
    const raw = await apiFetch<ApiSloDashboardResponse>(`/evaluation/slo/dashboard${qs}`);
    return {
      periodDays: raw.period_days,
      agents: raw.agents.map((a) => ({
        agentId: a.agent_id,
        sloName: a.slo_name,
        target: a.target,
        actual: a.actual,
        met: a.met,
      })),
    };
  } catch {
    return null;
  }
}

// ── Memory health & metrics ─────────────────────────────────────────────────

type ApiMemoryHealthResponse = {
  total_memories: number;
  knowledge_entries: number;
  last_governance_run: string;
  healthy: boolean;
};

type ApiMemoryMetricsResponse = {
  total_memories: number;
  avg_confidence: number;
  stale_count: number;
};

export async function getMemoryHealth(): Promise<MemoryHealthData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const raw = await apiFetch<ApiMemoryHealthResponse>('/evaluation/memory-health');
    return {
      totalMemories: raw.total_memories,
      knowledgeEntries: raw.knowledge_entries,
      lastGovernanceRun: raw.last_governance_run,
      healthy: raw.healthy,
    };
  } catch {
    return null;
  }
}

export async function getMemoryMetrics(): Promise<MemoryMetricsData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const raw = await apiFetch<ApiMemoryMetricsResponse>('/evaluation/memory-metrics');
    return {
      totalMemories: raw.total_memories,
      avgConfidence: raw.avg_confidence,
      staleCount: raw.stale_count,
    };
  } catch {
    return null;
  }
}
