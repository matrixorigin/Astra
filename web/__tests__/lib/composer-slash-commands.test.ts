import {
  filterSlashCommands,
  skillToSlashCommand,
  type SlashCommandItem,
} from '@/lib/composer/slash-commands';
import type { SkillSummary } from '@/lib/api/types';

function skill(name: string, description: string | null = null): SkillSummary {
  return {
    id: name,
    name,
    version: '1',
    description,
    source: 'local',
    category: 'test',
    status: 'active',
  };
}

describe('composer slash commands', () => {
  it('maps skills into generic slash command items', () => {
    expect(skillToSlashCommand(skill('skill-creator', 'Creates skills'))).toMatchObject({
      id: 'skill:skill-creator',
      kind: 'skill',
      value: 'skill-creator',
      label: 'skill-creator',
      description: 'Creates skills',
    });
  });

  it('filters commands by prefix rather than arbitrary substring', () => {
    const commands = [
      skillToSlashCommand(skill('skill-creator')),
      skillToSlashCommand(skill('canvas-design')),
      skillToSlashCommand(skill('review-code')),
    ];

    expect(filterSlashCommands(commands, 'skill')).toEqual([commands[0]]);
    expect(filterSlashCommands(commands, 'creator')).toEqual([]);
  });

  it('supports non-skill command kinds without changing filtering behavior', () => {
    const plan: SlashCommandItem = {
      id: 'mode:plan',
      kind: 'mode',
      value: 'plan',
      label: 'plan',
      description: 'Plan before acting',
    };
    const commands = [skillToSlashCommand(skill('skill-creator')), plan];

    expect(filterSlashCommands(commands, 'pl')).toEqual([plan]);
  });

  it('prioritizes direct command prefixes before keyword prefixes', () => {
    const commands: SlashCommandItem[] = [
      {
        id: 'skill:db-migrator',
        kind: 'skill',
        value: 'db-migrator',
        label: 'db-migrator',
        keywords: ['plan'],
      },
      {
        id: 'mode:plan',
        kind: 'mode',
        value: 'plan',
        label: 'plan',
      },
    ];

    expect(filterSlashCommands(commands, 'pl')).toEqual([commands[1], commands[0]]);
  });
});
