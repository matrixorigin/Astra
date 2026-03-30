import Link from 'next/link';
import { getWebConfigurationMessage } from '@/lib/api/client';
import { getModels } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

function formatTokens(n: number | null): string {
  if (n == null) return '—';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(0)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
  return String(n);
}

export default async function ModelsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Models" description="Registered model configurations from the backend.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const models = await getModels();

  return (
    <SectionCard
      title="Models"
      description={`${models.length} model${models.length === 1 ? '' : 's'} registered.`}
    >
      {mode === 'demo' ? (
        <div className="mb-5">
          <StatusCallout title="Demo data mode" message={config.message} />
        </div>
      ) : null}

      {models.length === 0 ? (
        <div className="rounded-2xl border border-dashed border-slate-700 px-6 py-12 text-center">
          <p className="text-sm text-slate-400">No models registered yet.</p>
          <p className="mt-1 text-xs text-slate-500">
            Models will appear here once configured in the backend.
          </p>
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-left text-sm">
            <thead>
              <tr className="border-b border-slate-700 text-xs uppercase tracking-wider text-slate-400">
                <th className="pb-3 pr-4 font-medium">Name</th>
                <th className="pb-3 pr-4 font-medium">Provider</th>
                <th className="pb-3 pr-4 font-medium">Status</th>
                <th className="pb-3 pr-4 font-medium">Context</th>
                <th className="pb-3 pr-4 font-medium">Pricing</th>
                <th className="pb-3 font-medium">Modalities</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-800">
              {models.map((model) => (
                <tr key={model.modelId} className="group transition hover:bg-slate-800/40">
                  <td className="py-3 pr-4">
                    <Link
                      href={`/models/${encodeURIComponent(model.name)}`}
                      className="font-medium text-sky-300 hover:text-sky-200"
                    >
                      {model.name}
                    </Link>
                  </td>
                  <td className="py-3 pr-4 text-slate-300">{model.provider}</td>
                  <td className="py-3 pr-4">
                    {model.isActive ? (
                      <span className="inline-flex items-center rounded-full bg-emerald-400/10 px-2.5 py-0.5 text-xs font-medium text-emerald-400 ring-1 ring-inset ring-emerald-400/30">
                        Active
                      </span>
                    ) : (
                      <span className="inline-flex items-center rounded-full bg-slate-400/10 px-2.5 py-0.5 text-xs font-medium text-slate-400 ring-1 ring-inset ring-slate-400/30">
                        Inactive
                      </span>
                    )}
                  </td>
                  <td className="py-3 pr-4 font-mono text-xs text-slate-300">
                    {formatTokens(model.contextWindow)}
                  </td>
                  <td className="py-3 pr-4 font-mono text-xs text-slate-300">
                    ${model.pricing.prompt} / ${model.pricing.completion}
                  </td>
                  <td className="py-3">
                    <div className="flex flex-wrap gap-1">
                      {model.inputModalities.map((m) => (
                        <span
                          key={`in-${m}`}
                          className="rounded bg-slate-700/60 px-1.5 py-0.5 text-xs text-slate-300"
                        >
                          {m}
                        </span>
                      ))}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </SectionCard>
  );
}
