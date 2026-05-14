import Link from 'next/link';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils/cn';

type CardProps = {
  children: ReactNode;
  className?: string;
  href?: string;
  interactive?: boolean;
};

export function Card({ children, className, href, interactive }: CardProps) {
  const classes = cn(
    'relative rounded-card border border-border bg-surface p-4',
    interactive && 'hover:border-border-strong hover:bg-surface-muted',
    className,
  );

  if (href) {
    return (
      <Link href={href} className={classes}>
        {children}
      </Link>
    );
  }

  return <div className={classes}>{children}</div>;
}
