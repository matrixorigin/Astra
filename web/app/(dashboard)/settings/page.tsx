import { SectionCard } from '@/components/dashboard/section-card';
import { RuntimeSettingsPanel } from '@/components/settings/runtime-settings-panel';

export default function SettingsPage() {
  return (
    <SectionCard
      title="Settings"
      description="Manage backend connection, authentication, and runtime tokens."
    >
      <RuntimeSettingsPanel />
    </SectionCard>
  );
}
