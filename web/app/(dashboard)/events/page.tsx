import { getWebConfigurationMessage } from '@/lib/api/client';
import { getEvents } from '@/lib/api/platform';
import { SectionCard } from '@/components/dashboard/section-card';
import { StatusCallout } from '@/components/dashboard/status-callout';
import { EventLogViewer } from '@/components/events/event-log-viewer';
import { getRuntimeConfig } from '@/lib/runtime-config';

export const dynamic = 'force-dynamic';

export default async function EventsPage() {
  const config = await getRuntimeConfig();
  const mode = config.mode;

  if (mode === 'unconfigured') {
    return (
      <SectionCard title="Events" description="Inspect recent event records from the backend.">
        <StatusCallout
          title="Frontend API not configured"
          message={getWebConfigurationMessage()}
          tone="warning"
        />
      </SectionCard>
    );
  }

  const events = await getEvents(50);

  return (
    <SectionCard
      title="Events"
      description={`${events.length} event records loaded. Filter by type or search by content.`}
    >
      {mode === 'demo' ? (
        <div className="mb-5">
          <StatusCallout
            title="Demo data mode"
            message={config.message}
          />
        </div>
      ) : null}

      <EventLogViewer
        events={events}
        emptyMessage="No events found matching the current filters."
      />
    </SectionCard>
  );
}
