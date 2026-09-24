import { requestJson } from '@/lib/api/request';
import type {
  HarnessDecisionRequest,
  AuthoringIntentRecord,
  AuthoringIntentRequest,
  HarnessItem,
  HarnessNodeCatalogItem,
  HarnessRun,
  HarnessSkillDraft,
  HarnessTemplate,
  SkillifyPublishRecord,
  SkillifyPublishRequest,
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

export function createAuthoringIntent(payload: AuthoringIntentRequest, sessionId?: string) {
  const path = sessionId
    ? `/api/chats/${encodeURIComponent(sessionId)}/authoring`
    : '/api/authoring';
  return requestJson<AuthoringIntentRecord>(
    path,
    {
      method: 'POST',
      body: JSON.stringify(payload),
    },
  );
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

export function listSkillDrafts(runId: string) {
  return requestJson<HarnessSkillDraft[]>(
    `/api/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts`,
  );
}

export function decideSkillDraft(runId: string, draftId: string, payload: HarnessDecisionRequest) {
  return requestJson<HarnessSkillDraft>(
    `/api/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts/${encodeURIComponent(draftId)}/decision`,
    {
      method: 'POST',
      body: JSON.stringify(payload),
    },
  );
}

export function decideSkillRule(
  runId: string,
  draftId: string,
  ruleId: string,
  payload: HarnessDecisionRequest,
) {
  return requestJson<HarnessSkillDraft>(
    `/api/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts/${encodeURIComponent(draftId)}/rules/${encodeURIComponent(ruleId)}/decision`,
    {
      method: 'POST',
      body: JSON.stringify(payload),
    },
  );
}

export function publishSkillDraft(runId: string, draftId: string, payload: SkillifyPublishRequest) {
  return requestJson<SkillifyPublishRecord>(
    `/api/harnesses/runs/${encodeURIComponent(runId)}/skill-drafts/${encodeURIComponent(draftId)}/publish`,
    {
      method: 'POST',
      body: JSON.stringify(payload),
    },
  );
}


export function listAuthoringTargets(sessionId: string) {
  return requestJson<Array<NonNullable<AuthoringIntentRequest['target_skill']>>>(`/api/chats/${encodeURIComponent(sessionId)}/authoring`);
}

export function activatePersonalSkill(skillName: string, sessionId: string, versionId: string, expectedVersionId: string | null) {
  return requestJson<{ version_id: string; content_hash: string }>(`/api/skills/user/${encodeURIComponent(skillName)}/activate`, {
    method: 'POST', body: JSON.stringify({ session_id: sessionId, version_id: versionId, expected_active_version_id: expectedVersionId }),
  });
}

/** Recover through authorized durable reads, never by starting authoring again. */
export async function loadAuthoringResult(runId: string): Promise<AuthoringIntentRecord> {
  const [run, drafts] = await Promise.all([getHarnessRun(runId), listSkillDrafts(runId)]);
  const authoring = run.output_json.authoring as (Partial<AuthoringIntentRecord> & { evaluated_draft_revision?: number }) | undefined;
  if (!authoring?.inference || !authoring.evaluation) {
    throw new Error(run.error ?? '生成尚未完成，请稍后恢复已有结果。');
  }
  const stale = !!authoring.evaluation_plan && (drafts.length !== 1 || drafts[0].revision !== authoring.evaluated_draft_revision);
  const sources = run.input_json.source_packets as Array<{ title: string; content: string }> | undefined;
  return {
    target: 'skill', operation: authoring.operation ?? 'create', resolution_source: 'persisted_authoring',
    goal: sources?.find((source) => source.title === 'authoring-intent.txt')?.content ?? '',
    harness_run: run, skill_drafts: drafts, inference: authoring.inference,
    evaluation: stale ? { status: 'unavailable', reason: '正文已改变；旧评估仅适用于先前版本。', experiment_id: null } : authoring.evaluation,
    evaluation_plan: stale ? null : authoring.evaluation_plan ?? null,
  };
}
