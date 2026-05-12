import { splitSkillMentions } from '@/lib/composer/skill-mentions';
import { cn } from '@/lib/utils/cn';

type SkillMentionTextProps = {
  content: string;
  skills?: string[];
  className?: string;
};

export function SkillMentionText({ content, skills, className }: SkillMentionTextProps) {
  const parts = splitSkillMentions(content, skills);

  return (
    <p className={cn('whitespace-pre-wrap break-words text-base leading-7 text-text', className)}>
      {parts.map((part, index) => {
        if (part.kind === 'text') {
          return <span key={`${index}:text`}>{part.text}</span>;
        }
        return (
          <span
            key={`${index}:${part.skillName}`}
            className="mx-0.5 inline-flex max-w-full rounded px-0.5 text-accent"
            title={`Skill: ${part.skillName}`}
          >
            {part.text}
          </span>
        );
      })}
    </p>
  );
}
