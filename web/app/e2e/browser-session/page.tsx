import { notFound } from 'next/navigation';
import { getCurrentUser } from '@/lib/auth/actions';

export const dynamic = 'force-dynamic';

export default async function BrowserSessionRender() {
  if (process.env.ASTRA_ENABLE_E2E_PAGES !== '1') notFound();
  const user = await getCurrentUser();
  return <main>{user ? `Authenticated: ${user.user_id}` : 'Not authenticated'}</main>;
}
