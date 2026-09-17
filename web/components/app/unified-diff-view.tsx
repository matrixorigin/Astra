"use client";

import { useMemo, useState, type UIEvent } from "react";
import { cn } from "@/lib/utils/cn";

const LINE_HEIGHT = 22;
const VIEWPORT_HEIGHT = 484;
const OVERSCAN_LINES = 12;
const CHECKPOINT_INTERVAL = 256;

type DiffKind = "file" | "meta" | "hunk" | "context" | "addition" | "deletion";

type ParserState = {
  inHunk: boolean;
  oldLine: number;
  newLine: number;
};

type DiffIndex = {
  lineCount: number;
  checkpoints: Array<{ offset: number; state: ParserState }>;
};

type VisibleDiffLine = {
  index: number;
  text: string;
  kind: DiffKind;
  oldLine: number | null;
  newLine: number | null;
};

const HUNK_HEADER = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/u;

function readLine(data: string, offset: number): { text: string; nextOffset: number } {
  const newline = data.indexOf("\n", offset);
  const rawEnd = newline === -1 ? data.length : newline;
  const end = rawEnd > offset && data.charCodeAt(rawEnd - 1) === 13 ? rawEnd - 1 : rawEnd;
  return {
    text: data.slice(offset, end),
    nextOffset: newline === -1 ? data.length : newline + 1,
  };
}

function advanceParser(state: ParserState, text: string): Omit<VisibleDiffLine, "index" | "text"> {
  if (text.startsWith("diff --git ")) {
    state.inHunk = false;
    return { kind: "file", oldLine: null, newLine: null };
  }
  const hunk = HUNK_HEADER.exec(text);
  if (hunk) {
    state.inHunk = true;
    state.oldLine = Number(hunk[1]);
    state.newLine = Number(hunk[2]);
    return { kind: "hunk", oldLine: null, newLine: null };
  }
  if (!state.inHunk) {
    return { kind: "meta", oldLine: null, newLine: null };
  }
  if (text.startsWith("+")) {
    const newLine = state.newLine;
    state.newLine += 1;
    return { kind: "addition", oldLine: null, newLine };
  }
  if (text.startsWith("-")) {
    const oldLine = state.oldLine;
    state.oldLine += 1;
    return { kind: "deletion", oldLine, newLine: null };
  }
  if (text.startsWith(" ")) {
    const oldLine = state.oldLine;
    const newLine = state.newLine;
    state.oldLine += 1;
    state.newLine += 1;
    return { kind: "context", oldLine, newLine };
  }
  return { kind: "meta", oldLine: null, newLine: null };
}

function buildDiffIndex(data: string): DiffIndex {
  const checkpoints: DiffIndex["checkpoints"] = [];
  const state: ParserState = { inHunk: false, oldLine: 0, newLine: 0 };
  let offset = 0;
  let lineCount = 0;
  while (offset < data.length) {
    if (lineCount % CHECKPOINT_INTERVAL === 0) {
      checkpoints.push({ offset, state: { ...state } });
    }
    const line = readLine(data, offset);
    advanceParser(state, line.text);
    offset = line.nextOffset;
    lineCount += 1;
  }
  return { lineCount, checkpoints };
}

function visibleLines(
  data: string,
  index: DiffIndex,
  startLine: number,
  endLine: number,
): VisibleDiffLine[] {
  if (endLine <= startLine) return [];
  const checkpointIndex = Math.floor(startLine / CHECKPOINT_INTERVAL);
  const checkpointLine = checkpointIndex * CHECKPOINT_INTERVAL;
  const checkpoint = index.checkpoints[checkpointIndex]!;
  const state = { ...checkpoint.state };
  let offset = checkpoint.offset;
  const rows: VisibleDiffLine[] = [];
  for (let line = checkpointLine; line < endLine && offset < data.length; line += 1) {
    const current = readLine(data, offset);
    const parsed = advanceParser(state, current.text);
    if (line >= startLine) rows.push({ index: line, text: current.text, ...parsed });
    offset = current.nextOffset;
  }
  return rows;
}

const rowTone: Record<DiffKind, string> = {
  file: "bg-surface-muted text-text font-semibold",
  meta: "text-text-muted",
  hunk: "bg-accent/10 text-accent",
  context: "text-text-secondary",
  addition: "bg-success/10 text-text",
  deletion: "bg-danger/10 text-text",
};

export function UnifiedDiffView({ data }: { data: string }) {
  const index = useMemo(() => buildDiffIndex(data), [data]);
  const [scrollTop, setScrollTop] = useState(0);
  const visibleCount = Math.ceil(VIEWPORT_HEIGHT / LINE_HEIGHT);
  const startLine = Math.max(0, Math.floor(scrollTop / LINE_HEIGHT) - OVERSCAN_LINES);
  const endLine = Math.min(
    index.lineCount,
    startLine + visibleCount + OVERSCAN_LINES * 2,
  );
  const rows = visibleLines(data, index, startLine, endLine);

  function handleScroll(event: UIEvent<HTMLDivElement>) {
    setScrollTop(event.currentTarget.scrollTop);
  }

  return (
    <div
      className="relative max-h-[484px] overflow-auto border-t border-border/70 bg-bg font-mono text-[12px] leading-[22px]"
      style={{ height: Math.min(VIEWPORT_HEIGHT, index.lineCount * LINE_HEIGHT) }}
      onScroll={handleScroll}
      role="table"
      aria-label="Unified diff"
      aria-rowcount={index.lineCount}
    >
      <div
        className="relative min-w-full"
        style={{ height: index.lineCount * LINE_HEIGHT }}
      >
        {rows.map((row) => (
          <div
            key={row.index}
            role="row"
            data-diff-kind={row.kind}
            aria-rowindex={row.index + 1}
            className={cn(
              "absolute left-0 grid min-w-full w-max grid-cols-[3.25rem_3.25rem_minmax(max-content,1fr)] whitespace-pre",
              rowTone[row.kind],
            )}
            style={{ top: row.index * LINE_HEIGHT, height: LINE_HEIGHT }}
          >
            <span
              role="cell"
              aria-label={row.oldLine === null ? undefined : `Old line ${row.oldLine}`}
              className="select-none border-r border-border/50 px-2 text-right tabular-nums text-text-muted/70"
            >
              {row.oldLine ?? ""}
            </span>
            <span
              role="cell"
              aria-label={row.newLine === null ? undefined : `New line ${row.newLine}`}
              className="select-none border-r border-border/50 px-2 text-right tabular-nums text-text-muted/70"
            >
              {row.newLine ?? ""}
            </span>
            <span role="cell" className="block min-w-max px-3 pr-8">
              {row.text || " "}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
