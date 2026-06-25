import { requestJson, toQuery } from '@/lib/api/request';
import type { SkillListResponse, SkillSummary } from '@/lib/api/types';
import type { RuntimeSkillListCursor, RuntimeSkillListItem, RuntimeSkillListResponse } from '@astra/sdk';

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

export async function listSkills(params: { limit?: number; cursor?: RuntimeSkillListCursor | null } = {}) {
  const payload = await requestJson<RuntimeSkillListResponse>(
    `/api/skills${toQuery({
      limit: params.limit ?? 100,
      ...(params.cursor
        ? {
            after_skill_name: params.cursor.skill_name,
            after_version: params.cursor.version,
            after_skill_id: params.cursor.skill_id,
          }
        : {}),
    })}`,
  );
  const items = (payload.skills ?? [])
    .map(toSkillSummary)
    .filter((item): item is SkillSummary => item !== null);
  const total = payload.total ?? items.length;
  const limit = payload.limit ?? params.limit ?? items.length;

  return {
    items,
    total,
    limit,
    nextCursor: payload.next_cursor ?? null,
  } satisfies SkillListResponse;
}
