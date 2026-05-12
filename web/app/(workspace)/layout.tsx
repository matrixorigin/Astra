import type { ReactNode } from 'react';
import { AppShell } from '@/components/app/app-shell';
import { getCurrentUser } from '@/lib/auth/actions';

export default async function WorkspaceLayout({ children }: { children: ReactNode }) {
  const user = await getCurrentUser();
  if (!user) {
    return <>{children}</>;
  }

  return <AppShell>{children}</AppShell>;
}
