import type { ComponentType, ReactNode } from 'react';

export function EmptyState({
  icon: Icon,
  title,
  description,
  cta,
}: {
  icon: ComponentType<{ className?: string }>;
  title: string;
  description?: string;
  cta?: ReactNode;
}) {
  return (
    <div className="flex min-h-52 flex-col items-center justify-center rounded-card border border-dashed border-border bg-surface/70 p-8 text-center">
      <span className="flex size-10 items-center justify-center rounded-control bg-surface-muted text-text-muted">
        <Icon className="size-5" />
      </span>
      <h2 className="mt-4 text-sm font-semibold">{title}</h2>
      {description ? <p className="mt-2 max-w-sm text-sm leading-6 text-text-secondary">{description}</p> : null}
      {cta ? <div className="mt-5">{cta}</div> : null}
    </div>
  );
}
