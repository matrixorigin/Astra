'use client';

import * as Tooltip from '@radix-ui/react-tooltip';
import type { ComponentType } from 'react';
import { cn } from '@/lib/utils/cn';

type IconButtonProps = React.ButtonHTMLAttributes<HTMLButtonElement> & {
  icon: ComponentType<{ className?: string }>;
  label: string;
  active?: boolean;
  tooltip?: string;
};

export function IconButton({
  icon: Icon,
  label,
  tooltip,
  active,
  className,
  type = 'button',
  ...props
}: IconButtonProps) {
  const button = (
    <button
      {...props}
      type={type}
      aria-label={label}
      className={cn(
        'inline-flex size-9 shrink-0 items-center justify-center rounded-control text-text-secondary hover:bg-surface-muted hover:text-text disabled:pointer-events-none disabled:opacity-40',
        active && 'bg-accent-soft text-accent',
        className,
      )}
    >
      <Icon className="size-4" />
    </button>
  );

  if (!tooltip) {
    return button;
  }

  return (
    <Tooltip.Provider delayDuration={250}>
      <Tooltip.Root>
        <Tooltip.Trigger asChild>{button}</Tooltip.Trigger>
        <Tooltip.Portal>
          <Tooltip.Content
            side="right"
            className="z-50 rounded-control bg-text px-2 py-1 text-xs text-white shadow-lg"
          >
            {tooltip}
            <Tooltip.Arrow className="fill-text" />
          </Tooltip.Content>
        </Tooltip.Portal>
      </Tooltip.Root>
    </Tooltip.Provider>
  );
}
