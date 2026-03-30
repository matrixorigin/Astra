import { apiFetch, tryApiFetch, getWebDataMode } from '@/lib/api/client';

type ApiTaskListResponse = {
  tasks: Record<string, unknown>[];
  total: number;
};

type ApiTaskProgressResponse = {
  task: Record<string, unknown>;
  progress_events: {
    subtask_id: string;
    subtask_title: string;
    action: string;
    progress_pct: number;
    total_subtasks: number;
    completed_subtasks: number;
    timestamp: string;
  }[];
};

export async function getTasks(
  statusFilter?: string,
): Promise<{ tasks: Record<string, unknown>[]; total: number }> {
  const mode = await getWebDataMode();
  if (mode !== 'live') {
    return { tasks: [], total: 0 };
  }
  const qs = statusFilter ? `?status=${statusFilter}` : '';
  const result = await tryApiFetch<ApiTaskListResponse>(`/tasks${qs}`);
  return result ?? { tasks: [], total: 0 };
}

export async function getTask(
  taskId: string,
): Promise<Record<string, unknown> | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    return await apiFetch<Record<string, unknown>>(`/tasks/${taskId}`);
  } catch {
    return null;
  }
}

export async function getTaskProgress(
  taskId: string,
): Promise<ApiTaskProgressResponse | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    return await apiFetch<ApiTaskProgressResponse>(`/tasks/${taskId}/progress`);
  } catch {
    return null;
  }
}
