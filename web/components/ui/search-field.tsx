'use client';

import { Search } from 'lucide-react';
import { cn } from '@/lib/utils/cn';

type SearchFieldProps = React.InputHTMLAttributes<HTMLInputElement> & {
  containerClassName?: string;
};

export function SearchField({ className, containerClassName, ...props }: SearchFieldProps) {
  return (
    <label
      className={cn(
        'flex h-11 items-center gap-3 rounded-control border border-border bg-surface px-3 focus-within:border-accent',
        containerClassName,
      )}
    >
      <Search className="size-4 shrink-0 text-text-muted" />
      <input
        {...props}
        className={cn(
          'min-w-0 flex-1 bg-transparent text-sm text-text outline-none placeholder:text-text-muted',
          className,
        )}
      />
    </label>
  );
}
