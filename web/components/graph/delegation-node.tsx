'use client';

import { memo } from 'react';
import { Handle, Position, type NodeProps } from '@xyflow/react';

const delegationStatusColors: Record<string, { bg: string; border: string; text: string }> = {
  delegated:   { bg: '#1e1b4b', border: '#8b5cf6', text: '#e0e7ff' },
  in_progress: { bg: '#2e1065', border: '#a78bfa', text: '#f5f3ff' },
  completed:   { bg: '#052e16', border: '#22c55e', text: '#dcfce7' },
  failed:      { bg: '#450a0a', border: '#ef4444', text: '#fee2e2' },
};

interface DelegationNodeData {
  label: string;
  fromAgent: string;
  toAgent: string;
  status: string;
  [key: string]: unknown;
}

function DelegationNodeInner({ data }: NodeProps) {
  const d = data as unknown as DelegationNodeData;
  const palette = delegationStatusColors[d.status] ?? delegationStatusColors.delegated;

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-purple-500 !w-2.5 !h-2.5 !border-purple-700" />
      <div
        className="rounded-xl border-2 border-dashed px-4 py-3 shadow-lg min-w-[220px] max-w-[280px]"
        style={{
          backgroundColor: palette.bg,
          borderColor: palette.border,
          color: palette.text,
        }}
      >
        <div className="flex items-center gap-1.5 text-xs opacity-70">
          <span>🤖</span>
          <span>{d.fromAgent}</span>
          <span>→</span>
          <span className="font-semibold">{d.toAgent}</span>
        </div>
        <p className="mt-1 text-sm font-medium leading-snug">{d.label}</p>
        <span className="mt-1 inline-block text-xs uppercase tracking-wide opacity-60">
          {d.status}
        </span>
      </div>
      <Handle type="source" position={Position.Bottom} className="!bg-purple-500 !w-2.5 !h-2.5 !border-purple-700" />
    </>
  );
}

export const DelegationNode = memo(DelegationNodeInner);
