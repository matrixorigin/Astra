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
    <div className="flex min-h-56 flex-col items-center justify-center rounded-card border border-dashed border-border bg-surface p-8 text-center">
      <Icon className="size-8 text-text-muted" />
      <h2 className="mt-4 text-base font-semibold">{title}</h2>
      {description ? <p className="mt-2 max-w-sm text-sm text-text-secondary">{description}</p> : null}
      {cta ? <div className="mt-5">{cta}</div> : null}
    </div>
  );
}
