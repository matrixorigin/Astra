import { tryApiFetch, getWebDataMode } from '@/lib/api/client';
import { mockPlatformSnapshot } from '@/lib/api/mock-data';
import type { EventSummary } from '@/lib/models/platform';
import type { ApiEventListResponse } from './platform-types';
import { normalizeEvent } from './platform-types';

export async function getEvents(limit = 50): Promise<EventSummary[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot.events;
  }

  const response = await tryApiFetch<ApiEventListResponse>(`/events?limit=${limit}`);
  return response ? response.events.map(normalizeEvent) : [];
}
