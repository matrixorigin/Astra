import { SectionCard } from '@/components/dashboard/section-card';
import { RuntimeSettingsPanel } from '@/components/settings/runtime-settings-panel';

export default function SettingsPage() {
  return (
    <SectionCard
      title="Settings"
      description="Configure API connectivity, login state, and saved runtime tokens for the frontend."
    >
      <RuntimeSettingsPanel />
    </SectionCard>
  );
}
