'use client';

import { useRef, useEffect, useState } from 'react';

type Props = {
  /** The bash command being executed. */
  command: string;
  /** Accumulated output lines (streamed incrementally). */
  output: string;
  /** Whether the command is still running. */
  isRunning: boolean;
  /** Exit code (undefined while running). */
  exitCode?: number;
  /** Max height for the terminal viewport (CSS value). */
  maxHeight?: string;
};

export default function TerminalViewer({
  command,
  output,
  isRunning,
  exitCode,
  maxHeight = '300px',
}: Props) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [autoScroll, setAutoScroll] = useState(true);

  // Auto-scroll to bottom as new output arrives.
  useEffect(() => {
    if (autoScroll && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [output, autoScroll]);

  // Detect manual scroll to disable auto-scroll.
  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    setAutoScroll(atBottom);
  };

  const statusColor = isRunning
    ? 'text-green-400'
    : exitCode === 0
      ? 'text-slate-400'
      : 'text-red-400';

  const statusText = isRunning
    ? '● Running'
    : exitCode === 0
      ? '✓ Completed'
      : `✗ Exit ${exitCode ?? '?'}`;

  return (
    <div className="rounded-lg border border-slate-700 bg-black overflow-hidden">
      {/* Header bar */}
      <div className="flex items-center justify-between px-3 py-1.5 bg-slate-800 border-b border-slate-700">
        <div className="flex items-center gap-2">
          <span className="text-sm">⬛</span>
          <span className="text-xs font-mono text-slate-300 truncate max-w-md">
            $ {command}
          </span>
        </div>
        <span className={`text-xs font-medium ${statusColor}`}>{statusText}</span>
      </div>

      {/* Terminal output */}
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="overflow-auto p-3"
        style={{ maxHeight }}
      >
        <pre className="text-xs font-mono text-green-300 whitespace-pre-wrap break-all m-0">
          {output}
          {isRunning && <span className="animate-pulse text-green-500">▌</span>}
        </pre>
      </div>

      {/* Auto-scroll indicator */}
      {!autoScroll && isRunning && (
        <div className="px-3 py-1 text-center border-t border-slate-800">
          <button
            onClick={() => {
              setAutoScroll(true);
              scrollRef.current?.scrollTo({
                top: scrollRef.current.scrollHeight,
                behavior: 'smooth',
              });
            }}
            className="text-xs text-slate-500 hover:text-slate-300 transition-colors"
          >
            ↓ Scroll to bottom
          </button>
        </div>
      )}
    </div>
  );
}
