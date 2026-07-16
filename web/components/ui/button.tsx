'use client';

import Link from 'next/link';
import type { ComponentType, ReactNode } from 'react';
import { cn } from '@/lib/utils/cn';

type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
type ButtonSize = 'sm' | 'md';

const variantClass: Record<ButtonVariant, string> = {
  primary: 'bg-text text-white shadow-sm hover:-translate-y-px hover:bg-text/90 disabled:bg-text/40',
  secondary: 'border border-border bg-surface text-text shadow-sm hover:border-border-strong hover:bg-surface-muted',
  ghost: 'text-text-secondary hover:bg-surface-muted hover:text-text',
  danger: 'bg-danger text-white shadow-sm hover:bg-danger/90',
};

const sizeClass: Record<ButtonSize, string> = {
  sm: 'h-8 px-3 text-sm',
  md: 'h-10 px-4 text-sm',
};

type SharedProps = {
  variant?: ButtonVariant;
  size?: ButtonSize;
  leadingIcon?: ComponentType<{ className?: string }>;
  trailingIcon?: ComponentType<{ className?: string }>;
  children: ReactNode;
  className?: string;
};

type ButtonProps = SharedProps & {
  href?: string;
  type?: 'button' | 'submit' | 'reset';
  disabled?: boolean;
  onClick?: React.MouseEventHandler<HTMLButtonElement>;
};

export function Button({
  variant = 'secondary',
  size = 'md',
  leadingIcon: LeadingIcon,
  trailingIcon: TrailingIcon,
  className,
  children,
  ...props
}: ButtonProps) {
  const classes = cn(
    'inline-flex shrink-0 items-center justify-center gap-2 rounded-control font-medium outline-none transition focus-visible:ring-4 focus-visible:ring-accent/15 disabled:pointer-events-none disabled:translate-y-0 disabled:opacity-60',
    variantClass[variant],
    sizeClass[size],
    className,
  );

  const content = (
    <>
      {LeadingIcon ? <LeadingIcon className="size-4" /> : null}
      <span className="truncate">{children}</span>
      {TrailingIcon ? <TrailingIcon className="size-4" /> : null}
    </>
  );

  if (props.href) {
    return (
      <Link href={props.href} className={classes}>
        {content}
      </Link>
    );
  }

  return (
    <button type={props.type ?? 'button'} disabled={props.disabled} onClick={props.onClick} className={classes}>
      {content}
    </button>
  );
}
