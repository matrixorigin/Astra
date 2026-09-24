import { EvaluationPage } from '@/components/app/evaluation-page';
import { getCurrentUser } from '@/lib/auth/actions';
import { getRuntimeConfig } from '@/lib/runtime-config';

export default async function Page() {
  const [user, runtime] = await Promise.all([getCurrentUser(), getRuntimeConfig()]);
  return (
    <EvaluationPage
      ownerId={user?.user_id ?? 'anonymous'}
      runtimeKey={runtime.apiUrl ?? 'default-runtime'}
    />
  );
}
