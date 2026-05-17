import { requestJson } from '@/lib/api/request';
import type {
  HarnessDecisionRequest,
  HarnessItem,
  HarnessNodeCatalogItem,
  HarnessRun,
  HarnessTemplate,
  SkillifyDraft,
  SkillifyDraftRequest,
  SkillifyRunRequest,
} from '@/lib/api/types';

export function listHarnessTemplates() {
  return requestJson<HarnessTemplate[]>('/api/harnesses/templates');
}

export function listHarnessNodeCatalog() {
  return requestJson<HarnessNodeCatalogItem[]>('/api/harnesses/node-catalog');
}

export function createSkillifyRun(payload: SkillifyRunRequest) {
  return requestJson<HarnessRun>('/api/harnesses/skillify/runs', {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}

export function getHarnessRun(runId: string) {
  return requestJson<HarnessRun>(`/api/harnesses/runs/${encodeURIComponent(runId)}`);
}

export function listHarnessRunItems(runId: string) {
  return requestJson<HarnessItem[]>(`/api/harnesses/runs/${encodeURIComponent(runId)}/items`);
}

export function decideHarnessItem(runId: string, itemId: string, payload: HarnessDecisionRequest) {
  return requestJson<HarnessItem>(
    `/api/harnesses/runs/${encodeURIComponent(runId)}/items/${encodeURIComponent(itemId)}/decision`,
    {
      method: 'POST',
      body: JSON.stringify(payload),
    },
  );
}

export function createSkillifyDraft(runId: string, payload: SkillifyDraftRequest) {
  return requestJson<SkillifyDraft>(`/api/harnesses/runs/${encodeURIComponent(runId)}/skillify/draft`, {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}
