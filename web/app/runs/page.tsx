import { getWebConfigurationMessage } from '@/lib/api/client';
import { getEvents } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function RunsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard
        title="Runs"
        description="The backend currently exposes single-run status endpoints, but this frontend still needs API configuration."
      >
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const events = await getEvents(12);

  return (
    <div className="space-y-6">
      <SectionCard
        title="Runs"
        description="Single-run APIs exist today, but the backend still lacks a first-class run list endpoint."
      >
        {mode === 'demo' ? (
          <div className="mb-5">
            <StatusCallout
              title="Demo data mode"
              message={config.message}
            />
          </div>
        ) : null}

        <StatusCallout
          title="Current backend gap"
          message="`GET /chat/runs/{run_id}` and `/chat/runs/{run_id}/stream` exist, but there is no global list endpoint yet. This page currently surfaces recent run-like activity from the event stream instead of pretending a complete run registry already exists."
          tone="warning"
        />
      </SectionCard>

      <SectionCard
        title="Recent run-like events"
        description="These events are the closest stable backend signal for current run lifecycle activity."
      >
        <div className="space-y-3">
          {events.map((event) => (
            <div key={event.id} className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div>
                  <p className="font-medium text-white">{event.type}</p>
                  <p className="mt-1 text-sm text-slate-400">{event.summary}</p>
                </div>
                <div className="text-right text-xs text-slate-500">
                  <p>{event.createdAt}</p>
                  <p>{event.sessionId}</p>
                </div>
              </div>
            </div>
          ))}
        </div>
      </SectionCard>
    </div>
  );
}
