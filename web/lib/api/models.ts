import { requestJson } from '@/lib/api/request';
import type { RuntimeModelAccessView } from '@astra/sdk';
import type { ModelSummary } from '@/lib/api/types';

export type ModelCatalogResponse = {
  items: ModelSummary[];
  accesses: RuntimeModelAccessView[];
  defaultOfferingId: string | null;
  catalogRevision: string;
  observedAt: string;
  source: 'astra';
  status: 'ready' | 'unavailable';
  actions: Array<'contact_administrator' | 'reconnect_device'>;
};

export function listModels() {
  return requestJson<ModelCatalogResponse>('/api/models');
}
