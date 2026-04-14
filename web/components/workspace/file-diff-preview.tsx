'use client';

import { useMemo } from 'react';

type DiffLine = {
  type: 'add' | 'remove' | 'context';
  content: string;
  oldLine?: number;
  newLine?: number;
};

type Props = {
  /** File path being modified. */
  path: string;
  /** The old (before) content. */
  oldContent?: string;
  /** The new (after) content. */
  newContent?: string;
  /** Pre-computed unified diff string (if available instead of old/new content). */
  unifiedDiff?: string;
  /** Maximum number of lines to show. */
  maxLines?: number;
};

function parseDiffLines(oldContent: string, newContent: string): DiffLine[] {
  const oldLines = oldContent.split('\n');
  const newLines = newContent.split('\n');
  const result: DiffLine[] = [];
  let oldIdx = 0;
  let newIdx = 0;

  // Simple line-by-line diff (LCS would be better but this is a scaffold)
  while (oldIdx < oldLines.length || newIdx < newLines.length) {
    if (oldIdx < oldLines.length && newIdx < newLines.length) {
      if (oldLines[oldIdx] === newLines[newIdx]) {
        result.push({
          type: 'context',
          content: oldLines[oldIdx],
          oldLine: oldIdx + 1,
          newLine: newIdx + 1,
        });
        oldIdx++;
        newIdx++;
      } else {
        result.push({
          type: 'remove',
          content: oldLines[oldIdx],
          oldLine: oldIdx + 1,
        });
        oldIdx++;
        result.push({
          type: 'add',
          content: newLines[newIdx],
          newLine: newIdx + 1,
        });
        newIdx++;
      }
    } else if (oldIdx < oldLines.length) {
      result.push({
        type: 'remove',
        content: oldLines[oldIdx],
        oldLine: oldIdx + 1,
      });
      oldIdx++;
    } else {
      result.push({
        type: 'add',
        content: newLines[newIdx],
        newLine: newIdx + 1,
      });
      newIdx++;
    }
  }
  return result;
}

function parseUnifiedDiff(diff: string): DiffLine[] {
  const lines = diff.split('\n');
  const result: DiffLine[] = [];
  let oldLine = 0;
  let newLine = 0;

  for (const line of lines) {
    if (line.startsWith('@@')) {
      const match = line.match(/@@ -(\d+)/);
      if (match) {
        oldLine = parseInt(match[1], 10) - 1;
        newLine = oldLine;
      }
      continue;
    }
    if (line.startsWith('---') || line.startsWith('+++')) continue;

    if (line.startsWith('+')) {
      newLine++;
      result.push({ type: 'add', content: line.slice(1), newLine });
    } else if (line.startsWith('-')) {
      oldLine++;
      result.push({ type: 'remove', content: line.slice(1), oldLine });
    } else {
      oldLine++;
      newLine++;
      result.push({
        type: 'context',
        content: line.startsWith(' ') ? line.slice(1) : line,
        oldLine,
        newLine,
      });
    }
  }
  return result;
}

const LINE_STYLES = {
  add: 'bg-green-900/40 text-green-300',
  remove: 'bg-red-900/40 text-red-300',
  context: 'text-slate-400',
} as const;

const LINE_PREFIXES = {
  add: '+',
  remove: '-',
  context: ' ',
} as const;

export default function FileDiffPreview({
  path,
  oldContent,
  newContent,
  unifiedDiff,
  maxLines = 200,
}: Props) {
  const diffLines = useMemo(() => {
    if (unifiedDiff) return parseUnifiedDiff(unifiedDiff);
    if (oldContent !== undefined && newContent !== undefined) {
      return parseDiffLines(oldContent, newContent);
    }
    return [];
  }, [oldContent, newContent, unifiedDiff]);

  const truncated = diffLines.length > maxLines;
  const visibleLines = truncated ? diffLines.slice(0, maxLines) : diffLines;
  const additions = diffLines.filter((l) => l.type === 'add').length;
  const deletions = diffLines.filter((l) => l.type === 'remove').length;

  return (
    <div className="rounded-lg border border-slate-700 bg-slate-900 overflow-hidden">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 bg-slate-800 border-b border-slate-700">
        <div className="flex items-center gap-2">
          <span className="text-sm">📄</span>
          <span className="text-sm font-mono text-slate-300">{path}</span>
        </div>
        <div className="flex gap-2 text-xs">
          {additions > 0 && (
            <span className="text-green-400">+{additions}</span>
          )}
          {deletions > 0 && (
            <span className="text-red-400">-{deletions}</span>
          )}
        </div>
      </div>

      {/* Diff lines */}
      <div className="overflow-x-auto">
        <pre className="text-xs font-mono leading-5 p-0 m-0">
          {visibleLines.map((line, i) => (
            <div key={i} className={`px-3 ${LINE_STYLES[line.type]}`}>
              <span className="inline-block w-8 text-right text-slate-600 mr-2 select-none">
                {line.oldLine ?? ''}
              </span>
              <span className="inline-block w-8 text-right text-slate-600 mr-2 select-none">
                {line.newLine ?? ''}
              </span>
              <span className="text-slate-600 mr-1 select-none">
                {LINE_PREFIXES[line.type]}
              </span>
              {line.content}
            </div>
          ))}
        </pre>
      </div>

      {/* Truncation notice */}
      {truncated && (
        <div className="px-3 py-2 text-center text-xs text-slate-500 bg-slate-800 border-t border-slate-700">
          Showing {maxLines} of {diffLines.length} lines
        </div>
      )}
    </div>
  );
}
