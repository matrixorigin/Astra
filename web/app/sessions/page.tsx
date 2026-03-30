import { getWebConfigurationMessage } from '@/lib/api/client';
import { getSessions } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { SessionsTableClient } from '@/components/sessions/sessions-table-client';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function SessionsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Sessions" description="List current session containers from the backend.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const sessions = await getSessions();

  return (
    <SectionCard
      title="Sessions"
      description="User-facing threads and resumable conversation containers."
    >
      {mode === 'demo' ? (
        <div className="mb-5">
          <StatusCallout
            title="Demo data mode"
            message={config.message}
          />
        </div>
      ) : null}

      <SessionsTableClient sessions={sessions} />
    </SectionCard>
  );
}
