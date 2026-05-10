'use client';

import * as Dropdown from '@radix-ui/react-dropdown-menu';
import type { ComponentType, ReactNode } from 'react';
import { useState } from 'react';
import { cn } from '@/lib/utils/cn';

export function Menu({
  trigger,
  children,
  onOpenChange,
}: {
  trigger: ReactNode;
  children: ReactNode;
  onOpenChange?: (open: boolean) => void;
}) {
  return (
    <Dropdown.Root onOpenChange={onOpenChange}>
      <Dropdown.Trigger asChild>{trigger}</Dropdown.Trigger>
      <Dropdown.Portal>
        <Dropdown.Content
          align="end"
          sideOffset={8}
          className="z-50 min-w-52 rounded-card border border-border bg-surface p-1 shadow-xl"
        >
          {children}
        </Dropdown.Content>
      </Dropdown.Portal>
    </Dropdown.Root>
  );
}

export function MenuItem({
  icon: Icon,
  children,
  destructive,
  disabled,
  onSelect,
}: {
  icon?: ComponentType<{ className?: string }>;
  children: ReactNode;
  destructive?: boolean;
  disabled?: boolean;
  onSelect?: (event: Event) => void;
}) {
  return (
    <Dropdown.Item
      disabled={disabled}
      onSelect={onSelect}
      className={cn(
        'flex cursor-default items-center gap-2 rounded-control px-3 py-2 text-sm outline-none hover:bg-surface-muted focus:bg-surface-muted disabled:pointer-events-none disabled:opacity-40',
        destructive && 'text-danger',
      )}
    >
      {Icon ? <Icon className="size-4" /> : null}
      {children}
    </Dropdown.Item>
  );
}

export function MenuConfirmPanel({
  message,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  destructive,
  busy,
  onCancel,
  onConfirm,
}: {
  message: string;
  confirmLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
  busy?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="rounded-control px-3 py-2">
      <p className="text-sm leading-relaxed text-text-muted">{message}</p>
      <div className="mt-3 flex items-center justify-end gap-2">
        <button
          type="button"
          disabled={busy}
          onClick={onCancel}
          className="rounded-control px-3 py-1.5 text-sm text-text-muted hover:bg-surface-muted hover:text-text disabled:opacity-50"
        >
          {cancelLabel}
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={onConfirm}
          className={cn(
            'rounded-control px-3 py-1.5 text-sm font-medium disabled:opacity-50',
            destructive
              ? 'bg-danger text-white hover:bg-danger/90'
              : 'bg-text text-white hover:bg-text/90',
          )}
        >
          {confirmLabel}
        </button>
      </div>
    </div>
  );
}

export function ConfirmMenuItem({
  icon: Icon,
  children,
  message,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  destructive,
  disabled,
  onConfirm,
}: {
  icon?: ComponentType<{ className?: string }>;
  children: ReactNode;
  message: string;
  confirmLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
  disabled?: boolean;
  onConfirm: () => void;
}) {
  const [confirming, setConfirming] = useState(false);

  if (confirming) {
    return (
      <div className="rounded-control px-3 py-2">
        <p className="text-xs leading-relaxed text-text-muted">{message}</p>
        <div className="mt-2 flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={() => setConfirming(false)}
            className="rounded-control px-2 py-1 text-xs text-text-muted hover:bg-surface-muted hover:text-text"
          >
            {cancelLabel}
          </button>
          <button
            type="button"
            onClick={onConfirm}
            className={cn(
              'rounded-control px-2 py-1 text-xs font-medium',
              destructive
                ? 'bg-danger text-white hover:bg-danger/90'
                : 'bg-text text-white hover:bg-text/90',
            )}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    );
  }

  return (
    <MenuItem
      icon={Icon}
      destructive={destructive}
      disabled={disabled}
      onSelect={(event) => {
        event.preventDefault();
        setConfirming(true);
      }}
    >
      {children}
    </MenuItem>
  );
}
