import { cn } from '@/lib/utils/cn';

export function Input({ className, ...props }: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      {...props}
      className={cn(
        'h-10 w-full rounded-control border border-border bg-surface px-3 text-sm text-text placeholder:text-text-muted outline-none focus:border-accent',
        className,
      )}
    />
  );
}
