import { getCurrentUser } from '@/lib/auth/actions';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { Suspense } from 'react';
import { HarnessesPage } from '@/components/app/harnesses-page';

export default async function Page() {
  const [user, runtime] = await Promise.all([getCurrentUser(), getRuntimeConfig()]);
  return <Suspense><HarnessesPage ownerId={user?.user_id ?? 'anonymous'} runtimeKey={runtime.apiUrl ?? 'default-runtime'} /></Suspense>;
}
