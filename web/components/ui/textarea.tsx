import { cn } from '@/lib/utils/cn';

export function Textarea({ className, ...props }: React.TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea
      {...props}
      className={cn(
        'min-h-24 w-full resize-none rounded-control border border-border bg-surface px-3 py-2 text-sm text-text placeholder:text-text-muted outline-none focus:border-accent',
        className,
      )}
    />
  );
}
