'use client';

import { memo } from 'react';
import { Handle, Position, type NodeProps } from '@xyflow/react';
import { statusColors, effortBadge } from '@/lib/graph/layout';

interface SubtaskNodeData {
  label: string;
  status: string;
  effort?: string;
  filesCount: number;
  description?: string;
  acceptance?: string;
  [key: string]: unknown;
}

function SubtaskNodeInner({ data }: NodeProps) {
  const d = data as unknown as SubtaskNodeData;
  const palette = statusColors[d.status] ?? statusColors.pending;

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-slate-500 !w-2.5 !h-2.5 !border-slate-700" />
      <div
        className="rounded-xl border-2 px-4 py-3 shadow-lg min-w-[220px] max-w-[280px]"
        style={{
          backgroundColor: palette.bg,
          borderColor: palette.border,
          color: palette.text,
        }}
      >
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs font-semibold uppercase tracking-wide opacity-70">
            {d.status.replace('_', ' ')}
          </span>
          {d.effort && (
            <span className="text-xs">{effortBadge[d.effort] ?? d.effort}</span>
          )}
        </div>
        <p className="mt-1 text-sm font-medium leading-snug">{d.label}</p>
        {d.filesCount > 0 && (
          <p className="mt-1 text-xs opacity-60">
            {d.filesCount} file{d.filesCount > 1 ? 's' : ''}
          </p>
        )}
      </div>
      <Handle type="source" position={Position.Bottom} className="!bg-slate-500 !w-2.5 !h-2.5 !border-slate-700" />
    </>
  );
}

export const SubtaskNode = memo(SubtaskNodeInner);
