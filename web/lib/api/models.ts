import { requestJson } from '@/lib/api/request';
import type { ModelSummary } from '@/lib/api/types';

export function listModels() {
  return requestJson<{ items: ModelSummary[] }>('/api/models');
}
