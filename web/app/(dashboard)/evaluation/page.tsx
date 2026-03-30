import { getWebConfigurationMessage } from '@/lib/api/client';
import {
  getDriftSignals,
  getMemoryHealth,
  getMemoryMetrics,
  getQualityTrend,
  getSloDashboard,
} from '@/lib/api/platform';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { QualityTrendChart } from '@/components/evaluation/quality-trend-chart';
import { DriftSignalsPanel } from '@/components/evaluation/drift-signals-panel';
import { SloComplianceTable } from '@/components/evaluation/slo-compliance-table';
import { MemoryHealthCard } from '@/components/evaluation/memory-health-card';

export const dynamic = 'force-dynamic';

export default async function EvaluationPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard
        title="Evaluation & Quality"
        description="Quality trends, drift detection, SLO compliance, and memory health."
      >
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const [qualityTrend, drift, slo, memHealth, memMetrics] = await Promise.all([
    getQualityTrend().catch(() => null),
    getDriftSignals().catch(() => null),
    getSloDashboard().catch(() => null),
    getMemoryHealth().catch(() => null),
    getMemoryMetrics().catch(() => null),
  ]);

  return (
    <div className="space-y-6">
      {/* Hero */}
      <div>
        <h1 className="text-3xl font-semibold text-white">Evaluation &amp; Quality</h1>
        <p className="mt-2 max-w-2xl text-sm leading-6 text-slate-400">
          Quality trends, drift detection, SLO compliance, and memory health across your agents.
        </p>
      </div>

      {mode === 'demo' && <StatusCallout title="Demo data mode" message={config.message} />}

      {/* Quality trend – full width */}
      <SectionCard
        title="Quality trend"
        description="Average quality score over time across evaluation events."
      >
        {qualityTrend ? (
          <QualityTrendChart data={qualityTrend} />
        ) : (
          <p className="py-6 text-center text-sm text-slate-500">
            Quality trend data is not available.
          </p>
        )}
      </SectionCard>

      {/* Drift + SLO – 2 column grid */}
      <div className="grid gap-6 lg:grid-cols-2">
        <SectionCard
          title="Drift signals"
          description="Model quality drift detection across templates."
        >
          {drift ? (
            <DriftSignalsPanel data={drift} />
          ) : (
            <p className="py-6 text-center text-sm text-slate-500">
              Drift data is not available.
            </p>
          )}
        </SectionCard>

        <SectionCard
          title="SLO compliance"
          description="Service-level objective tracking per agent."
        >
          {slo ? (
            <SloComplianceTable data={slo} />
          ) : (
            <p className="py-6 text-center text-sm text-slate-500">
              SLO data is not available.
            </p>
          )}
        </SectionCard>
      </div>

      {/* Memory health */}
      <SectionCard
        title="Memory health"
        description="Aggregate memory health and metrics."
      >
        <MemoryHealthCard health={memHealth} metrics={memMetrics} />
      </SectionCard>
    </div>
  );
}
