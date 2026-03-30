import { tryApiFetch, getWebDataMode } from '@/lib/api/client';
import { mockRunList } from '@/lib/api/mock-data';
import type { RunListData, RunSummary } from '@/lib/models/platform';
import type { ApiRunStatus, ApiRunListResponse } from './platform-types';

function normalizeRun(run: ApiRunStatus): RunSummary {
  return {
    runId: run.run_id,
    sessionId: run.session_id,
    status: run.status,
    waitingFor: run.waiting_for,
    eventsCount: run.events_count,
  };
}

export async function getRuns(limit = 50, offset = 0): Promise<RunListData> {
  if ((await getWebDataMode()) === 'demo') {
    return mockRunList;
  }

  const response = await tryApiFetch<ApiRunListResponse>(`/runs?limit=${limit}&offset=${offset}`);
  if (!response) {
    return { runs: [], total: 0, limit, offset };
  }
  return {
    runs: response.runs.map(normalizeRun),
    total: response.total,
    limit: response.limit,
    offset: response.offset,
  };
}
