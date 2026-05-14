import { normalizeSkillMentionNames, splitSkillMentions } from '@/lib/composer/skill-mentions';

describe('skill mention rendering helpers', () => {
  it('normalizes selected skills without retaining slash prefixes', () => {
    expect(normalizeSkillMentionNames(['/skill-creator', ' canvas-design ', 'skill-creator'])).toEqual([
      'skill-creator',
      'canvas-design',
    ]);
  });

  it('splits only the selected skill mentions in user text', () => {
    expect(splitSkillMentions('/skill-creator ask /canvas-design next', [
      'skill-creator',
      'canvas-design',
    ])).toEqual([
      { kind: 'skill', text: '/skill-creator', skillName: 'skill-creator' },
      { kind: 'text', text: ' ask ' },
      { kind: 'skill', text: '/canvas-design', skillName: 'canvas-design' },
      { kind: 'text', text: ' next' },
    ]);
  });

  it('does not style partial skill-name prefixes', () => {
    expect(splitSkillMentions('/skill-creator-extra', ['skill-creator'])).toEqual([
      { kind: 'text', text: '/skill-creator-extra' },
    ]);
  });
});
