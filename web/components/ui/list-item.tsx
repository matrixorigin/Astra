import Link from 'next/link';
import type { ComponentType, ReactNode } from 'react';
import { cn } from '@/lib/utils/cn';

type ListItemProps = {
  href?: string;
  icon?: ComponentType<{ className?: string }>;
  title: ReactNode;
  subtitle?: ReactNode;
  active?: boolean;
  trailing?: ReactNode;
  onClick?: () => void;
  className?: string;
};

export function ListItem({
  href,
  icon: Icon,
  title,
  subtitle,
  active,
  trailing,
  onClick,
  className,
}: ListItemProps) {
  const content = (
    <>
      {Icon ? <Icon className="mt-0.5 size-4 shrink-0 text-text-muted" /> : null}
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">{title}</span>
        {subtitle ? <span className="block truncate text-xs text-text-muted">{subtitle}</span> : null}
      </span>
      {trailing}
    </>
  );
  const classes = cn(
    'flex w-full items-start gap-3 rounded-control px-3 py-2 text-left text-text-secondary hover:bg-surface-muted hover:text-text',
    active && 'bg-accent-soft text-accent',
    className,
  );

  if (href) {
    return (
      <Link href={href} className={classes} onClick={onClick}>
        {content}
      </Link>
    );
  }

  return (
    <button type="button" className={classes} onClick={onClick}>
      {content}
    </button>
  );
}
