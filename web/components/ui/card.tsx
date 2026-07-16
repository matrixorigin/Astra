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
    'relative rounded-card border border-border/80 bg-surface p-4 shadow-[0_1px_2px_rgba(15,23,42,0.025)]',
    interactive &&
      'transition hover:-translate-y-px hover:border-border-strong hover:bg-surface-raised hover:shadow-[0_10px_28px_rgba(15,23,42,0.07)] focus-visible:outline-none focus-visible:ring-4 focus-visible:ring-accent/10',
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
