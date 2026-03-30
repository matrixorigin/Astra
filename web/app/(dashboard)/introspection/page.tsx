import Link from 'next/link';
import { getWebConfigurationMessage } from '@/lib/api/client';
import { getSkillsIntrospection } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { SkillsIntrospectionPanel } from '@/components/sessions/skills-introspection-panel';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function IntrospectionPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard
        title="Introspection"
        description="Inspect agent memory, skills, and context state."
      >
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const skills = await getSkillsIntrospection().catch(() => null);

  return (
    <div className="space-y-6">
      <SectionCard
        title="Introspection"
        description="Inspect agent memory, skills, and context state. Per-session memory and decision trace are available on individual session detail pages."
      >
        {mode === 'demo' && <StatusCallout title="Demo data mode" message={config.message} />}

        <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
            <h3 className="text-sm font-medium text-slate-300">Memory</h3>
            <p className="mt-1 text-xs text-slate-500">
              Episodic, semantic, and procedural memory stats are shown per session.
              Open any <Link href="/sessions" className="text-sky-400 hover:underline">session detail</Link> to inspect.
            </p>
          </div>
          <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
            <h3 className="text-sm font-medium text-slate-300">Decision trace</h3>
            <p className="mt-1 text-xs text-slate-500">
              Root-cause analysis and tool selection insights.
              Available on each <Link href="/sessions" className="text-sky-400 hover:underline">session detail</Link> page.
            </p>
          </div>
          <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
            <h3 className="text-sm font-medium text-slate-300">Context</h3>
            <p className="mt-1 text-xs text-slate-500">
              Context trend, snapshots, and retrieval quality are being wired.
            </p>
          </div>
        </div>
      </SectionCard>

      <SectionCard
        title="Skills"
        description="Installed and cloud-available skills for the current user."
      >
        {skills ? (
          <SkillsIntrospectionPanel data={skills} />
        ) : (
          <p className="py-6 text-center text-sm text-slate-500">
            {mode === 'demo'
              ? 'Skills introspection is not available in demo mode.'
              : 'Could not load skills data. The introspection service may not be configured.'}
          </p>
        )}
      </SectionCard>
    </div>
  );
}
