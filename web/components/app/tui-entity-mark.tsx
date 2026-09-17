import { cn } from '@/lib/utils/cn';

type TuiEntityMarkKind = 'chat' | 'now' | 'work' | 'project' | 'search' | 'harness' | 'new';

const MARKS: Record<TuiEntityMarkKind, string> = {
  chat: 'C',
  now: 'N',
  work: 'W',
  project: 'P',
  search: 'S',
  harness: 'H',
  new: '+',
};

export function TuiEntityMark({
  kind,
  className,
}: {
  kind: TuiEntityMarkKind;
  className?: string;
}) {
  return (
    <span className={cn('astra-tui-entity-mark', className)} aria-hidden="true">
      {MARKS[kind]}
    </span>
  );
}
