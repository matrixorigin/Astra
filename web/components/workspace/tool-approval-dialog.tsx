'use client';

import { useState, useEffect, useCallback } from 'react';

export type ToolApprovalRequest = {
  requestId: string;
  tool: string;
  args: Record<string, unknown>;
};

type Props = {
  request: ToolApprovalRequest;
  onApprove: (requestId: string) => void;
  onDeny: (requestId: string, reason?: string) => void;
  timeoutMs?: number;
};

const DANGEROUS_TOOLS = new Set([
  'bash',
  'write_file',
  'str_replace',
  'delete_file',
  'git_commit',
]);

function riskLevel(tool: string): 'low' | 'medium' | 'high' {
  if (tool === 'delete_file' || tool === 'bash') return 'high';
  if (DANGEROUS_TOOLS.has(tool)) return 'medium';
  return 'low';
}

const RISK_COLORS = {
  low: 'border-slate-600 bg-slate-800',
  medium: 'border-amber-600 bg-amber-950',
  high: 'border-red-600 bg-red-950',
} as const;

const RISK_LABELS = {
  low: { text: 'Low Risk', color: 'text-slate-400' },
  medium: { text: 'Review Recommended', color: 'text-amber-400' },
  high: { text: 'Potentially Dangerous', color: 'text-red-400' },
} as const;

export default function ToolApprovalDialog({
  request,
  onApprove,
  onDeny,
  timeoutMs = 60_000,
}: Props) {
  const [remaining, setRemaining] = useState(Math.ceil(timeoutMs / 1000));
  const [denyReason, setDenyReason] = useState('');
  const [showReason, setShowReason] = useState(false);

  useEffect(() => {
    if (remaining <= 0) {
      onDeny(request.requestId, 'Timed out');
      return;
    }
    const timer = setTimeout(() => setRemaining((r) => r - 1), 1000);
    return () => clearTimeout(timer);
  }, [remaining, request.requestId, onDeny]);

  const handleApprove = useCallback(() => {
    onApprove(request.requestId);
  }, [request.requestId, onApprove]);

  const handleDeny = useCallback(() => {
    onDeny(request.requestId, denyReason || undefined);
  }, [request.requestId, denyReason, onDeny]);

  const risk = riskLevel(request.tool);

  return (
    <div
      className={`rounded-lg border p-4 shadow-lg ${RISK_COLORS[risk]} max-w-lg`}
      role="alertdialog"
      aria-label={`Approve tool: ${request.tool}`}
    >
      {/* Header */}
      <div className="flex items-center justify-between mb-3">
        <div className="flex items-center gap-2">
          <span className="text-lg">🔐</span>
          <h3 className="text-sm font-semibold text-white">Tool Approval Required</h3>
        </div>
        <span className={`text-xs font-medium ${RISK_LABELS[risk].color}`}>
          {RISK_LABELS[risk].text} · {remaining}s
        </span>
      </div>

      {/* Tool info */}
      <div className="mb-3 rounded bg-black/30 p-3">
        <div className="text-xs text-slate-400 mb-1">Tool</div>
        <div className="text-sm font-mono text-white">{request.tool}</div>
        <div className="text-xs text-slate-400 mt-2 mb-1">Arguments</div>
        <pre className="text-xs font-mono text-slate-300 overflow-x-auto max-h-48 overflow-y-auto">
          {JSON.stringify(request.args, null, 2)}
        </pre>
      </div>

      {/* Deny reason (expandable) */}
      {showReason && (
        <div className="mb-3">
          <input
            type="text"
            value={denyReason}
            onChange={(e) => setDenyReason(e.target.value)}
            placeholder="Reason for denial (optional)"
            className="w-full rounded bg-black/30 border border-slate-600 px-3 py-1.5 text-sm text-white placeholder-slate-500 focus:border-slate-400 focus:outline-none"
          />
        </div>
      )}

      {/* Actions */}
      <div className="flex items-center gap-2">
        <button
          onClick={handleApprove}
          className="flex-1 rounded bg-green-700 hover:bg-green-600 px-3 py-1.5 text-sm font-medium text-white transition-colors"
        >
          ✓ Approve
        </button>
        <button
          onClick={() => (showReason ? handleDeny() : setShowReason(true))}
          className="flex-1 rounded bg-red-800 hover:bg-red-700 px-3 py-1.5 text-sm font-medium text-white transition-colors"
        >
          ✗ Deny
        </button>
      </div>

      {/* Timeout progress bar */}
      <div className="mt-3 h-1 rounded-full bg-black/30 overflow-hidden">
        <div
          className="h-full bg-slate-500 transition-all duration-1000 ease-linear"
          style={{ width: `${(remaining / (timeoutMs / 1000)) * 100}%` }}
        />
      </div>
    </div>
  );
}
