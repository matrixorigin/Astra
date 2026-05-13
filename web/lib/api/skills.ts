import { requestJson, toQuery } from '@/lib/api/request';
import type { SkillListResponse, SkillSummary } from '@/lib/api/types';
import type { RuntimeSkillListItem, RuntimeSkillListResponse } from '@astra/sdk';

function toSkillSummary(skill: RuntimeSkillListItem): SkillSummary | null {
  const name = skill.skill_name?.trim();
  if (!name) {
    return null;
  }

  return {
    id: skill.skill_id || name,
    name,
    version: skill.version || '',
    description: skill.description ?? null,
    source: skill.source ?? null,
    category: skill.category ?? null,
    status: skill.status ?? null,
  };
}

export async function listSkills(params: { limit?: number; offset?: number } = {}) {
  const payload = await requestJson<RuntimeSkillListResponse>(
    `/api/skills${toQuery({
      limit: params.limit ?? 100,
      offset: params.offset ?? 0,
    })}`,
  );
  const items = (payload.skills ?? [])
    .map(toSkillSummary)
    .filter((item): item is SkillSummary => item !== null);
  const total = payload.total ?? items.length;
  const limit = payload.limit ?? params.limit ?? items.length;
  const offset = payload.offset ?? params.offset ?? 0;
  const nextOffset = offset + limit < total ? offset + limit : null;

  return {
    items,
    total,
    limit,
    offset,
    nextOffset,
  } satisfies SkillListResponse;
}
