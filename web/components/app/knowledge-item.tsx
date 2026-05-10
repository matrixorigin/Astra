'use client';

import { CheckCircle2, FileText, Trash2 } from 'lucide-react';
import { IconButton } from '@/components/ui/icon-button';
import type { KnowledgeFile } from '@/lib/api/types';

function size(bytes: number) {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

export function KnowledgeItem({
  file,
  onRemove,
}: {
  file: KnowledgeFile;
  onRemove?: (id: string) => void;
}) {
  return (
    <div className="flex items-center gap-3 rounded-control border border-border px-3 py-2">
      <FileText className="size-4 text-text-muted" />
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium">{file.filename}</p>
        <p className="text-xs text-text-muted">{size(file.sizeBytes)}</p>
      </div>
      {file.indexStatus === 'indexed' ? <CheckCircle2 className="size-4 text-success" /> : null}
      {onRemove ? <IconButton icon={Trash2} label={`Remove ${file.filename}`} onClick={() => onRemove(file.id)} /> : null}
    </div>
  );
}
