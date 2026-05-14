export type SkillMentionPart =
  | { kind: 'text'; text: string }
  | { kind: 'skill'; text: string; skillName: string };

const SKILL_NAME_BOUNDARY = /[A-Za-z0-9_.-]/;

export function normalizeSkillMentionNames(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }

  const seen = new Set<string>();
  const result: string[] = [];
  for (const skill of skills) {
    const normalized = skill.trim().replace(/^\/+/, '');
    if (!normalized || seen.has(normalized)) {
      continue;
    }
    seen.add(normalized);
    result.push(normalized);
  }
  return result;
}

export function splitSkillMentions(content: string, skills?: string[]): SkillMentionPart[] {
  const names = normalizeSkillMentionNames(skills)
    .sort((left, right) => right.length - left.length);

  if (!content || names.length === 0) {
    return [{ kind: 'text', text: content }];
  }

  const loweredContent = content.toLowerCase();
  const candidates = names.map((name) => ({
    name,
    needle: `/${name.toLowerCase()}`,
  }));
  const parts: SkillMentionPart[] = [];
  let cursor = 0;
  let textStart = 0;

  while (cursor < content.length) {
    let matched: { name: string; length: number } | null = null;

    if (content[cursor] === '/') {
      for (const candidate of candidates) {
        if (!loweredContent.startsWith(candidate.needle, cursor)) {
          continue;
        }
        const end = cursor + candidate.needle.length;
        const next = content[end];
        if (next && SKILL_NAME_BOUNDARY.test(next)) {
          continue;
        }
        matched = { name: candidate.name, length: candidate.needle.length };
        break;
      }
    }

    if (!matched) {
      cursor += 1;
      continue;
    }

    if (textStart < cursor) {
      parts.push({ kind: 'text', text: content.slice(textStart, cursor) });
    }
    parts.push({
      kind: 'skill',
      text: content.slice(cursor, cursor + matched.length),
      skillName: matched.name,
    });
    cursor += matched.length;
    textStart = cursor;
  }

  if (textStart < content.length) {
    parts.push({ kind: 'text', text: content.slice(textStart) });
  }

  return parts.length ? parts : [{ kind: 'text', text: content }];
}
