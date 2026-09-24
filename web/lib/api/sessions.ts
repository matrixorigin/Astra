import { requestJson, toQuery } from '@/lib/api/request';
import type { RuntimeSessionListCursor, RuntimeSessionListResponse, RuntimeSessionResponse } from '@astra/sdk';

export function listSessions(cursor?: RuntimeSessionListCursor | null) {
  return requestJson<RuntimeSessionListResponse>(`/api/sessions${toQuery(cursor ? {
    after_updated_at: cursor.updated_at, after_session_id: cursor.session_id,
  } : {})}`);
}

export function createSession(title: string) {
  return requestJson<RuntimeSessionResponse>('/api/sessions', { method: 'POST', body: JSON.stringify({ title }) });
}
