'use client';

import { useState, useEffect, useCallback } from 'react';
import type { PlanGraphData } from '@/lib/graph/types';
import { demoPlanGraphData, buildPlanGraphData, buildPlanGraphFromPlan } from '@/lib/graph/data';
import { PlanGraph } from '@/components/graph/plan-graph';
import { ProgressTimeline } from '@/components/graph/progress-timeline';

type ViewMode = 'graph' | 'timeline';

type TaskEntry = {
  task_id: string;
  title: string;
  status: string;
  progress_pct: number;
};

export default function PlansPage() {
  const [tasks, setTasks] = useState<TaskEntry[]>([]);
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const [graphData, setGraphData] = useState<PlanGraphData | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>('graph');
  const [loading, setLoading] = useState(true);
  const [isDemo, setIsDemo] = useState(false);

  // Fetch task list from backend
  const loadTaskList = useCallback(async () => {
    setLoading(true);
    try {
      const configRes = await fetch('/api/runtime-config');
      const config = await configRes.json();

      if (config.mode === 'demo' || !config.apiUrl) {
        setIsDemo(true);
        setGraphData(demoPlanGraphData());
        setLoading(false);
        return;
      }

      const res = await fetch(`${config.apiUrl}/tasks`, {
        headers: config.token
          ? { Authorization: `Bearer ${config.token}` }
          : {},
      });

      if (res.ok) {
        const body = await res.json();
        const taskList = body.tasks ?? [];
        setTasks(taskList);
        if (taskList.length > 0) {
          // Auto-select most recent task
          setSelectedTaskId(taskList[0].task_id);
        } else {
          setIsDemo(true);
          setGraphData(demoPlanGraphData());
        }
      } else {
        setIsDemo(true);
        setGraphData(demoPlanGraphData());
      }
    } catch {
      setIsDemo(true);
      setGraphData(demoPlanGraphData());
    } finally {
      setLoading(false);
    }
  }, []);

  // Fetch progress for a specific task
  const loadTaskProgress = useCallback(
    async (taskId: string) => {
      try {
        const configRes = await fetch('/api/runtime-config');
        const config = await configRes.json();
        if (!config.apiUrl) return;

        const res = await fetch(`${config.apiUrl}/tasks/${taskId}/progress`, {
          headers: config.token
            ? { Authorization: `Bearer ${config.token}` }
            : {},
        });

        if (res.ok) {
          const body = await res.json();
          // Use buildPlanGraphData to normalize task + progress events
          const eventsRaw = (body.progress_events ?? []).map(
            (pe: Record<string, unknown>) => ({
              event_type: 'PlanProgress',
              ts: pe.timestamp,
              metadata: pe,
            }),
          );
          setGraphData(buildPlanGraphData(body.task, eventsRaw));
        } else {
          // Fallback: try fetching just the task
          const taskRes = await fetch(`${config.apiUrl}/tasks/${taskId}`, {
            headers: config.token
              ? { Authorization: `Bearer ${config.token}` }
              : {},
          });
          if (taskRes.ok) {
            const taskBody = await taskRes.json();
            setGraphData(buildPlanGraphFromPlan(taskBody));
          }
        }
      } catch {
        // Keep existing graphData on error
      }
    },
    [],
  );

  useEffect(() => {
    loadTaskList();
  }, [loadTaskList]);

  useEffect(() => {
    if (selectedTaskId && !isDemo) {
      loadTaskProgress(selectedTaskId);
    }
  }, [selectedTaskId, isDemo, loadTaskProgress]);

  if (loading) {
    return (
      <div className="space-y-4">
        <div className="animate-pulse rounded-xl bg-slate-800/50 h-6 w-48" />
        <div className="animate-pulse rounded-xl bg-slate-800/50 h-[70vh]" />
      </div>
    );
  }

  if (!graphData) {
    return (
      <div className="rounded-2xl border border-dashed border-slate-700 p-12 text-center">
        <p className="text-slate-400">No plan data available</p>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-xl font-semibold text-white">Plan Visualization</h1>
          <p className="text-sm text-slate-400">
            {isDemo
              ? 'Showing demo data — connect to a running backend to see real plans'
              : `Dependency graph and execution timeline for agent plans`}
          </p>
        </div>
        <div className="flex items-center gap-3">
          {/* Task selector when multiple tasks exist */}
          {tasks.length > 1 && (
            <select
              value={selectedTaskId ?? ''}
              onChange={(e) => setSelectedTaskId(e.target.value)}
              className="rounded-xl border border-slate-700 bg-slate-900/80 px-3 py-1.5 text-xs text-slate-300 focus:outline-none focus:ring-2 focus:ring-sky-500/40"
            >
              {tasks.map((t) => (
                <option key={t.task_id} value={t.task_id}>
                  {t.title} ({t.status} · {t.progress_pct}%)
                </option>
              ))}
            </select>
          )}
          <div className="flex items-center gap-1 rounded-xl border border-slate-700 bg-slate-900/80 p-1">
            <button
              type="button"
              onClick={() => setViewMode('graph')}
              className={`rounded-lg px-3 py-1.5 text-xs font-medium transition-colors ${
                viewMode === 'graph'
                  ? 'bg-sky-600 text-white'
                  : 'text-slate-400 hover:text-white'
              }`}
            >
              Graph
            </button>
            <button
              type="button"
              onClick={() => setViewMode('timeline')}
              className={`rounded-lg px-3 py-1.5 text-xs font-medium transition-colors ${
                viewMode === 'timeline'
                  ? 'bg-sky-600 text-white'
                  : 'text-slate-400 hover:text-white'
              }`}
            >
              Timeline
            </button>
          </div>
        </div>
      </div>

      {viewMode === 'graph' ? (
        <div className="h-[calc(100vh-10rem)] rounded-2xl border border-slate-800 bg-slate-950/70 overflow-hidden">
          <PlanGraph data={graphData} />
        </div>
      ) : (
        <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-6">
          <ProgressTimeline events={graphData.progressEvents} />
        </div>
      )}

      {/* Plan notes */}
      {graphData.task.plan?.notes && (
        <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
          <h3 className="text-xs font-medium text-slate-400 mb-2">Plan Notes</h3>
          <p className="text-sm text-slate-300 leading-relaxed">
            {graphData.task.plan.notes}
          </p>
        </div>
      )}

      {/* Delegations summary */}
      {graphData.delegations.length > 0 && (
        <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
          <h3 className="text-xs font-medium text-slate-400 mb-3">Agent Delegations</h3>
          <div className="space-y-2">
            {graphData.delegations.map((del) => (
              <div
                key={del.id}
                className="flex items-center justify-between rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-2.5"
              >
                <div className="flex items-center gap-2 text-sm">
                  <span className="text-purple-400">🤖</span>
                  <span className="text-slate-300">{del.fromAgentId}</span>
                  <span className="text-slate-600">→</span>
                  <span className="font-medium text-white">{del.toAgentId}</span>
                </div>
                <span
                  className={`text-xs font-medium capitalize ${
                    del.status === 'completed'
                      ? 'text-green-400'
                      : del.status === 'failed'
                        ? 'text-red-400'
                        : del.status === 'in_progress'
                          ? 'text-sky-400'
                          : 'text-purple-400'
                  }`}
                >
                  {del.status}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
