'use client';

import * as Dialog from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import type { ReactNode } from 'react';
import { IconButton } from '@/components/ui/icon-button';
import { cn } from '@/lib/utils/cn';

type ModalProps = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title?: string;
  children: ReactNode;
  width?: number;
  className?: string;
};

export function Modal({ open, onOpenChange, title, children, width = 560, className }: ModalProps) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/20" />
        <Dialog.Content
          className={cn(
            'fixed left-1/2 top-1/2 z-50 max-h-[85vh] w-[calc(100vw-32px)] -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-card border border-border bg-surface shadow-2xl',
            className,
          )}
          style={{ maxWidth: width }}
        >
          {title ? (
            <div className="flex h-14 items-center justify-between border-b border-border px-5">
              <Dialog.Title className="text-base font-semibold">{title}</Dialog.Title>
              <Dialog.Close asChild>
                <IconButton icon={X} label="Close modal" />
              </Dialog.Close>
            </div>
          ) : null}
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
