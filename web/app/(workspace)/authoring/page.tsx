import { AuthoringPage } from '@/components/app/authoring-page';
import { getCurrentUser } from '@/lib/auth/actions';
import { getRuntimeConfig } from '@/lib/runtime-config';

export default async function Page() {
  const [user, runtime] = await Promise.all([getCurrentUser(), getRuntimeConfig()]);
  return <AuthoringPage ownerId={user?.user_id ?? 'anonymous'} runtimeKey={runtime.apiUrl ?? 'default-runtime'} />;
}
