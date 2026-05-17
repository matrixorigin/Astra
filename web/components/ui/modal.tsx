'use client';

import * as Dialog from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import type { ReactNode } from 'react';
import { IconButton } from '@/components/ui/icon-button';
import { cn } from '@/lib/utils/cn';

type ModalProps = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  titleHidden?: boolean;
  description?: string;
  children: ReactNode;
  width?: number;
  className?: string;
};

export function Modal({
  open,
  onOpenChange,
  title,
  titleHidden = false,
  description,
  children,
  width = 560,
  className,
}: ModalProps) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/20" />
        <Dialog.Content
          className={cn(
            'fixed left-1/2 top-1/2 z-50 max-h-[85vh] w-[calc(100vw-32px)] -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-card border border-border bg-surface shadow-2xl',
            className,
          )}
          {...(description ? {} : { 'aria-describedby': undefined })}
          style={{ maxWidth: width }}
        >
          {titleHidden ? (
            <>
              <Dialog.Title className="sr-only">{title}</Dialog.Title>
              {description ? <Dialog.Description className="sr-only">{description}</Dialog.Description> : null}
            </>
          ) : (
            <div className="flex h-14 items-center justify-between border-b border-border px-5">
              <div className="min-w-0">
                <Dialog.Title className="truncate text-base font-semibold">{title}</Dialog.Title>
                {description ? (
                  <Dialog.Description className="mt-0.5 truncate text-xs text-text-muted">
                    {description}
                  </Dialog.Description>
                ) : null}
              </div>
              <Dialog.Close asChild>
                <IconButton icon={X} label="Close modal" />
              </Dialog.Close>
            </div>
          )}
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
