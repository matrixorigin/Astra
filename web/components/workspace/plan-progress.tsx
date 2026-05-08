'use client';

import type { PlanState } from '@/lib/workspace/types';

type TodoNode = PlanState['subtasks'][number] & {
  parent_id?: string;
  parentId?: string;
  parent_todo_id?: string;
  parentTodoId?: string;
  section?: string;
  depth?: number;
  summary?: string;
  children: TodoNode[];
};

function ProgressBar({ completed, total }: { completed: number; total: number }) {
  const pct = total === 0 ? 0 : Math.round((completed / total) * 100);
  return (
    <div className="flex items-center gap-2">
      <div className="h-1.5 flex-1 rounded-full bg-slate-800">
        <div
          className="h-full rounded-full bg-sky-500 transition-all duration-500"
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="text-[10px] text-slate-500">{pct}%</span>
    </div>
  );
}

const STATUS_ICONS: Record<string, { icon: string; color: string }> = {
  done: { icon: '✓', color: 'text-emerald-400' },
  running: { icon: '●', color: 'text-amber-400 animate-pulse' },
  pending: { icon: '○', color: 'text-slate-600' },
  error: { icon: '✗', color: 'text-red-400' },
};

function parentIdOf(task: TodoNode): string | undefined {
  return task.parent_id ?? task.parentId ?? task.parent_todo_id ?? task.parentTodoId;
}

function buildTodoTree(plan: PlanState): TodoNode[] {
  const nodes = new Map<string, TodoNode>();
  const roots: TodoNode[] = [];

  for (const subtask of plan.subtasks) {
    const raw = subtask as PlanState['subtasks'][number] & Partial<TodoNode>;
    nodes.set(subtask.id, {
      ...subtask,
      parent_id: raw.parent_id,
      parentId: raw.parentId,
      parent_todo_id: raw.parent_todo_id,
      parentTodoId: raw.parentTodoId,
      section: raw.section,
      depth: raw.depth,
      summary: raw.summary,
      children: [],
    });
  }

  for (const node of nodes.values()) {
    const parent = parentIdOf(node);
    if (parent && nodes.has(parent)) {
      nodes.get(parent)?.children.push(node);
    } else {
      roots.push(node);
    }
  }

  return roots;
}

function TodoTreeNode({ node, depth = 0 }: { node: TodoNode; depth?: number }) {
  const { icon, color } = STATUS_ICONS[node.status] ?? STATUS_ICONS.pending;
  const isActive = node.status === 'running';
  const visualDepth = Math.min(depth || node.depth || 0, 5);

  return (
    <div>
      <div
        className={`flex items-start gap-2 rounded-md px-2 py-1.5 text-xs ${
          isActive ? 'border border-amber-500/20 bg-amber-500/5' : ''
        }`}
        style={{ marginLeft: `${visualDepth * 14}px` }}
      >
        <span className={`mt-0.5 flex-shrink-0 font-mono text-[11px] ${color}`}>
          {icon}
        </span>
        <span className="min-w-0 flex-1">
          {node.section ? (
            <span className="mb-0.5 block text-[10px] uppercase text-slate-500">
              {node.section}
            </span>
          ) : null}
          <span
            className={
              node.status === 'done'
                ? 'block truncate text-slate-500 line-through'
                : isActive
                  ? 'block truncate text-amber-200'
                  : 'block truncate text-slate-300'
            }
            title={node.title}
          >
            {node.title}
          </span>
          {node.summary ? (
            <span className="mt-0.5 block truncate text-[11px] text-slate-500">
              {node.summary}
            </span>
          ) : null}
        </span>
      </div>
      {node.children.map((child) => (
        <TodoTreeNode key={child.id} node={child} depth={depth + 1} />
      ))}
    </div>
  );
}

export function PlanProgressPanel({ plan }: { plan: PlanState }) {
  const completed = plan.subtasks.filter((s) => s.status === 'done').length;
  const total = plan.subtasks.length;
  const roots = buildTodoTree(plan);

  return (
    <div className="space-y-3 p-3">
      {/* Plan header */}
      <div>
        <p className="text-xs font-medium uppercase tracking-wider text-slate-400">
          Plan/Todos
        </p>
        {plan.title && (
          <p className="mt-1 text-sm text-white">{plan.title}</p>
        )}
      </div>

      {/* Progress */}
      <div>
        <div className="mb-1 flex items-center justify-between text-[10px] text-slate-500">
          <span>{completed} / {total} tasks</span>
        </div>
        <ProgressBar completed={completed} total={total} />
      </div>

      {/* Subtask list */}
      <div className="space-y-1">
        {roots.map((root) => (
          <TodoTreeNode key={root.id} node={root} />
        ))}
      </div>
    </div>
  );
}
