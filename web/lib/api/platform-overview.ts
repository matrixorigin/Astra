import { tryApiFetch, getWebDataMode, type WebDataMode } from '@/lib/api/client';
import { mockPlatformSnapshot } from '@/lib/api/mock-data';
import type { OverviewData } from '@/lib/models/platform';
import type {
  ApiPlatformSnapshot,
  ApiHealthResponse,
  ApiAgentListResponse,
  ApiSessionListResponse,
  ApiEventListResponse,
} from './platform-types';
import {
  normalizeHealth,
  normalizeAgent,
  normalizeSession,
  normalizeEvent,
  buildOverviewData,
} from './platform-types';

export function getDemoDataMode(): WebDataMode {
  return 'demo';
}

export async function getOverviewData(): Promise<OverviewData> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot;
  }

  // Try the aggregated snapshot endpoint first (single round-trip).
  try {
    const snapshot = await tryApiFetch<ApiPlatformSnapshot>('/platform/snapshot');
    if (snapshot) {
      const health = normalizeHealth(snapshot.health);
      const agents = snapshot.agents.agents.map(normalizeAgent);
      const sessions = snapshot.sessions.sessions.map(normalizeSession);
      const events = snapshot.events.events.map(normalizeEvent);
      return buildOverviewData(health, agents, sessions, events);
    }
  } catch {
    // Fall back to individual endpoints if snapshot is unavailable.
  }

  const [health, agents, sessions, events] = await Promise.all([
    tryApiFetch<ApiHealthResponse>('/health'),
    tryApiFetch<ApiAgentListResponse>('/agents'),
    tryApiFetch<ApiSessionListResponse>('/sessions?limit=8'),
    tryApiFetch<ApiEventListResponse>('/events?limit=8'),
  ]);

  return buildOverviewData(
    health ? normalizeHealth(health) : { status: 'unknown', database: 'unknown', persistOk: 0, persistFail: 0 },
    agents ? agents.agents.map(normalizeAgent) : [],
    sessions ? sessions.sessions.map(normalizeSession) : [],
    events ? events.events.map(normalizeEvent) : [],
  );
}
