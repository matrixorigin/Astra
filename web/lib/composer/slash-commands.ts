import type { SkillSummary } from '@/lib/api/types';

export type SlashCommandKind = 'skill' | 'mode' | 'action';

export type SlashCommandItem = {
  id: string;
  kind: SlashCommandKind;
  value: string;
  label: string;
  description?: string | null;
  keywords?: string[];
};

function normalize(value: string) {
  return value.trim().toLowerCase();
}

function commandSearchKeys(command: SlashCommandItem) {
  return [
    command.label,
    command.value,
    command.description ?? '',
    ...(command.keywords ?? []),
  ].map(normalize).filter(Boolean);
}

export function skillToSlashCommand(skill: SkillSummary): SlashCommandItem {
  return {
    id: `skill:${skill.name}`,
    kind: 'skill',
    value: skill.name,
    label: skill.name,
    description: skill.description,
    keywords: [
      skill.source ?? '',
      skill.category ?? '',
      skill.status ?? '',
    ].filter(Boolean),
  };
}

export function filterSlashCommands(
  commands: readonly SlashCommandItem[],
  query: string,
  limit = 8,
) {
  const needle = normalize(query);
  if (!needle) {
    return commands.slice(0, limit);
  }

  return commands
    .map((command) => {
      const keys = commandSearchKeys(command);
      const label = normalize(command.label);
      const directPrefix = label.startsWith(needle) || normalize(command.value).startsWith(needle);
      const keywordPrefix = keys.some((key) => key.startsWith(needle));
      return {
        command,
        matched: directPrefix || keywordPrefix,
        rank: directPrefix ? 0 : 1,
      };
    })
    .filter((entry) => entry.matched)
    .sort((left, right) => left.rank - right.rank || left.command.label.localeCompare(right.command.label))
    .map((entry) => entry.command)
    .slice(0, limit);
}
