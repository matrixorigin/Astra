import { tryApiFetch, getWebDataMode } from '@/lib/api/client';
import { mockPlatformSnapshot } from '@/lib/api/mock-data';
import type { AgentSummary } from '@/lib/models/platform';
import type { ApiAgentListResponse } from './platform-types';
import { normalizeAgent } from './platform-types';

export async function getAgents(): Promise<AgentSummary[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot.agents;
  }

  const response = await tryApiFetch<ApiAgentListResponse>('/agents');
  return response ? response.agents.map(normalizeAgent) : [];
}
