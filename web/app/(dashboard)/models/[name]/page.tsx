import { notFound } from 'next/navigation';
import Link from 'next/link';
import { getModelDetail } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';

export const dynamic = 'force-dynamic';

function maskUrl(url: string): string {
  try {
    const parsed = new URL(url);
    return `${parsed.protocol}//${parsed.host}/***`;
  } catch {
    return '***';
  }
}

function formatTokens(n: number | null): string {
  if (n == null) return '—';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(0)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
  return String(n);
}

function QuirkBadge({ label, enabled }: { label: string; enabled: boolean }) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium ring-1 ring-inset ${
        enabled
          ? 'bg-amber-400/10 text-amber-400 ring-amber-400/30'
          : 'bg-slate-400/10 text-slate-500 ring-slate-400/20'
      }`}
    >
      {label}
    </span>
  );
}

export default async function ModelDetailPage({
  params,
}: {
  params: Promise<{ name: string }>;
}) {
  const { name } = await params;
  const model = await getModelDetail(name);

  if (!model) {
    notFound();
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-3">
        <Link
          href="/models"
          className="text-sm text-slate-400 transition hover:text-sky-300"
        >
          ← Models
        </Link>
      </div>

      <SectionCard
        title={model.name}
        description={model.description ?? `${model.provider} model configuration`}
      >
        <div className="grid gap-6 sm:grid-cols-2">
          {/* Basic info */}
          <div className="rounded-2xl border border-slate-700 bg-slate-800/30 p-5">
            <h3 className="mb-4 text-sm font-semibold uppercase tracking-wider text-slate-400">
              General
            </h3>
            <dl className="space-y-3 text-sm">
              <div className="flex justify-between">
                <dt className="text-slate-400">Provider</dt>
                <dd className="text-slate-200">{model.provider}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-slate-400">Status</dt>
                <dd>
                  {model.isActive ? (
                    <span className="text-emerald-400">Active</span>
                  ) : (
                    <span className="text-slate-500">Inactive</span>
                  )}
                </dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-slate-400">Base URL</dt>
                <dd className="font-mono text-xs text-slate-300">{maskUrl(model.baseUrl)}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-slate-400">Architecture</dt>
                <dd className="text-slate-200">{model.architecture ?? '—'}</dd>
              </div>
            </dl>
          </div>

          {/* Capacity */}
          <div className="rounded-2xl border border-slate-700 bg-slate-800/30 p-5">
            <h3 className="mb-4 text-sm font-semibold uppercase tracking-wider text-slate-400">
              Capacity
            </h3>
            <dl className="space-y-3 text-sm">
              <div className="flex justify-between">
                <dt className="text-slate-400">Context window</dt>
                <dd className="font-mono text-slate-200">{formatTokens(model.contextWindow)}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-slate-400">Max completion</dt>
                <dd className="font-mono text-slate-200">
                  {formatTokens(model.maxCompletionTokens)}
                </dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-slate-400">Prompt cost</dt>
                <dd className="font-mono text-slate-200">${model.pricing.prompt}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-slate-400">Completion cost</dt>
                <dd className="font-mono text-slate-200">${model.pricing.completion}</dd>
              </div>
            </dl>
          </div>

          {/* Modalities */}
          <div className="rounded-2xl border border-slate-700 bg-slate-800/30 p-5">
            <h3 className="mb-4 text-sm font-semibold uppercase tracking-wider text-slate-400">
              Modalities
            </h3>
            <div className="space-y-3">
              <div>
                <p className="mb-1 text-xs text-slate-500">Input</p>
                <div className="flex flex-wrap gap-1.5">
                  {model.inputModalities.length > 0 ? (
                    model.inputModalities.map((m) => (
                      <span
                        key={m}
                        className="rounded bg-sky-400/10 px-2 py-0.5 text-xs text-sky-300 ring-1 ring-sky-400/20"
                      >
                        {m}
                      </span>
                    ))
                  ) : (
                    <span className="text-xs text-slate-500">—</span>
                  )}
                </div>
              </div>
              <div>
                <p className="mb-1 text-xs text-slate-500">Output</p>
                <div className="flex flex-wrap gap-1.5">
                  {model.outputModalities.length > 0 ? (
                    model.outputModalities.map((m) => (
                      <span
                        key={m}
                        className="rounded bg-sky-400/10 px-2 py-0.5 text-xs text-sky-300 ring-1 ring-sky-400/20"
                      >
                        {m}
                      </span>
                    ))
                  ) : (
                    <span className="text-xs text-slate-500">—</span>
                  )}
                </div>
              </div>
              {model.tags.length > 0 && (
                <div>
                  <p className="mb-1 text-xs text-slate-500">Tags</p>
                  <div className="flex flex-wrap gap-1.5">
                    {model.tags.map((tag) => (
                      <span
                        key={tag}
                        className="rounded bg-slate-700/60 px-2 py-0.5 text-xs text-slate-300"
                      >
                        {tag}
                      </span>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>

          {/* Quirks */}
          <div className="rounded-2xl border border-slate-700 bg-slate-800/30 p-5">
            <h3 className="mb-4 text-sm font-semibold uppercase tracking-wider text-slate-400">
              Quirks
            </h3>
            <div className="flex flex-wrap gap-2">
              <QuirkBadge label="Preserve reasoning" enabled={model.quirks.preserveReasoningContent} />
              <QuirkBadge label="No parallel tools" enabled={model.quirks.noParallelToolCalls} />
              <QuirkBadge label="Tool choice required" enabled={model.quirks.toolChoiceRequired} />
              <QuirkBadge label="Strict tool IDs" enabled={model.quirks.strictToolCallIds} />
              <QuirkBadge label="No system msg" enabled={model.quirks.noSystemMessage} />
              <QuirkBadge label="System as user" enabled={model.quirks.systemAsUserPrefix} />
            </div>
          </div>
        </div>
      </SectionCard>
    </div>
  );
}
