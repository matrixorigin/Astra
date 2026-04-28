'use client';

import { useCallback, useEffect, useRef, useState } from 'react';

type PlanSummary = {
  plan_id: string;
  goal: string;
  progress_pct: number;
  subtask_count: number;
  status: string;
};

type Subtask = {
  id: string;
  title: string;
  description?: string | null;
  status: string;
  depends_on?: string[];
};

type PlanDetail = {
  plan_id: string;
  phase: string;
  goal: string;
  version: number;
  plan?: { subtasks: Subtask[]; notes?: string | null } | null;
};

type StepRun = {
  run_id: string;
  plan_id: string;
  subtask_id: string;
  attempt: number;
  status: string;
  session_id: string;
  started_at: string;
  finished_at?: string | null;
  request_id: string;
  error?: string | null;
};

async function backendFetch<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/backend${path}`, {
    cache: 'no-store',
    ...init,
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`${res.status} ${res.statusText}: ${body}`);
  }
  const ct = res.headers.get('content-type') ?? '';
  if (ct.includes('application/json')) {
    return (await res.json()) as T;
  }
  return (await res.text()) as unknown as T;
}

export default function PlansManagePage() {
  const [plans, setPlans] = useState<PlanSummary[]>([]);
  const [selected, setSelected] = useState<PlanDetail | null>(null);
  const [runs, setRuns] = useState<StepRun[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [rewindAnchor, setRewindAnchor] = useState('');
  const [redoSubtask, setRedoSubtask] = useState('');
  // Modal state for delete confirm — replaces the browser `confirm()` modal
  // so the prompt lives in our design system and is themable/testable.
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  // `selected` is referenced inside `loadPlans` for the "auto-select first
  // plan only when nothing is selected" guard. Tracking via ref keeps the
  // callback stable (deps=[]) AND current — the alternative (adding
  // `selected` to deps) re-runs loadPlans on every selection change, which
  // causes a fetch storm.
  const selectedRef = useRef<PlanDetail | null>(null);
  useEffect(() => {
    selectedRef.current = selected;
  }, [selected]);

  const loadPlan = useCallback(async (planId: string) => {
    try {
      const detail = await backendFetch<PlanDetail>(`/plans/${planId}`);
      setSelected(detail);
      const runsBody = await backendFetch<{ runs: StepRun[] }>(
        `/plans/${planId}/step-runs?limit=200`,
      );
      setRuns(runsBody.runs ?? []);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const loadPlans = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const body = await backendFetch<{ plans: PlanSummary[] }>('/plans');
      setPlans(body.plans ?? []);
      // Auto-select first plan only on initial load, not every refresh.
      if ((body.plans ?? []).length > 0 && !selectedRef.current) {
        await loadPlan(body.plans[0].plan_id);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [loadPlan]);

  useEffect(() => {
    loadPlans();
  }, [loadPlans]);

  async function doAction(
    label: string,
    path: string,
    body: Record<string, unknown>,
  ) {
    if (!selected) return;
    setBusyAction(label);
    setError(null);
    try {
      await backendFetch(`/plans/${selected.plan_id}/${path}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          ...body,
          expected_version: selected.version,
        }),
      });
      await loadPlan(selected.plan_id);
      await loadPlans();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusyAction(null);
    }
  }

  async function approve() {
    await doAction('approve', 'exit-plan-mode', { approved: true });
  }

  async function reject() {
    await doAction('reject', 'exit-plan-mode', { approved: false });
  }

  async function rewind() {
    if (!rewindAnchor.trim()) return;
    await doAction('rewind', 'rewind', { anchor: rewindAnchor.trim() });
    setRewindAnchor('');
  }

  async function redo() {
    if (!redoSubtask.trim()) return;
    await doAction('redo', 'redo-step', { subtask_id: redoSubtask.trim() });
    setRedoSubtask('');
  }

  // Open the confirm modal; actual delete runs in performDelete once the
  // user clicks through. Keeps the destructive action two-click and
  // themable instead of relying on the browser's blocking `confirm()`.
  function del(planId: string) {
    setConfirmDelete(planId);
  }

  async function performDelete(planId: string) {
    setConfirmDelete(null);
    setBusyAction(`delete-${planId}`);
    setError(null);
    try {
      await backendFetch(`/plans/${planId}`, { method: 'DELETE' });
      if (selected?.plan_id === planId) {
        setSelected(null);
        setRuns([]);
      }
      await loadPlans();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusyAction(null);
    }
  }

  if (loading && plans.length === 0) {
    return (
      <div className="space-y-4">
        <div className="animate-pulse rounded-xl bg-slate-800/50 h-6 w-48" />
        <div className="animate-pulse rounded-xl bg-slate-800/50 h-[70vh]" />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-xl font-semibold text-white">Plan Management</h1>
        <p className="text-sm text-slate-400">
          Author, approve, rewind, and redo plans. Backed by the cloud-authoritative
          <code className="mx-1 rounded bg-slate-800 px-1">plans</code> and
          <code className="mx-1 rounded bg-slate-800 px-1">plan_step_runs</code>
          tables.
        </p>
      </div>

      {error && (
        <div className="rounded-xl border border-red-700 bg-red-950/40 p-3 text-sm text-red-300">
          {error}
        </div>
      )}

      <div className="grid grid-cols-12 gap-4">
        {/* Plan list */}
        <div className="col-span-4 rounded-2xl border border-slate-800 bg-slate-950/70 p-3">
          <h2 className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-400">
            Plans ({plans.length})
          </h2>
          <div className="space-y-1">
            {plans.length === 0 && (
              <p className="text-sm text-slate-500">No plans yet.</p>
            )}
            {plans.map((p) => (
              <button
                key={p.plan_id}
                type="button"
                onClick={() => loadPlan(p.plan_id)}
                className={`w-full rounded-lg border px-3 py-2 text-left text-sm transition-colors ${
                  selected?.plan_id === p.plan_id
                    ? 'border-sky-500 bg-sky-950/40 text-white'
                    : 'border-slate-700 bg-slate-900/50 text-slate-300 hover:border-slate-600'
                }`}
              >
                <div className="flex items-center justify-between">
                  <span className="truncate font-medium">{p.goal}</span>
                  <span className="ml-2 text-xs text-slate-500">
                    {p.progress_pct}%
                  </span>
                </div>
                <div className="mt-1 flex items-center justify-between text-xs text-slate-500">
                  <span>
                    {p.status} · {p.subtask_count} steps
                  </span>
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation();
                      del(p.plan_id);
                    }}
                    disabled={busyAction === `delete-${p.plan_id}`}
                    className="text-xs text-red-400 hover:text-red-300 disabled:opacity-50"
                  >
                    delete
                  </button>
                </div>
              </button>
            ))}
          </div>
        </div>

        {/* Plan detail */}
        <div className="col-span-8 space-y-4">
          {selected ? (
            <>
              <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
                <div className="flex items-start justify-between gap-4">
                  <div>
                    <h2 className="text-lg font-semibold text-white">
                      {selected.goal}
                    </h2>
                    <p className="mt-1 text-xs text-slate-500">
                      plan_id=<code>{selected.plan_id}</code> · phase=
                      <span
                        className={`font-medium ${
                          selected.phase === 'executing'
                            ? 'text-sky-400'
                            : selected.phase === 'completed'
                              ? 'text-green-400'
                              : 'text-amber-400'
                        }`}
                      >
                        {selected.phase}
                      </span>{' '}
                      · v{selected.version}
                    </p>
                  </div>
                  <div className="flex gap-2">
                    <button
                      type="button"
                      onClick={approve}
                      disabled={busyAction !== null}
                      className="rounded-lg border border-green-700 bg-green-950/40 px-3 py-1.5 text-xs text-green-300 hover:bg-green-900/40 disabled:opacity-50"
                    >
                      Approve
                    </button>
                    <button
                      type="button"
                      onClick={reject}
                      disabled={busyAction !== null}
                      className="rounded-lg border border-amber-700 bg-amber-950/40 px-3 py-1.5 text-xs text-amber-300 hover:bg-amber-900/40 disabled:opacity-50"
                    >
                      Reject
                    </button>
                  </div>
                </div>

                {/* Rewind / Redo controls */}
                <div className="mt-4 grid grid-cols-2 gap-3">
                  <div>
                    <label
                      htmlFor="rewind-anchor"
                      className="mb-1 block text-xs font-medium text-slate-400"
                    >
                      Rewind (anchor = 1-based index or subtask id prefix)
                    </label>
                    <div className="flex gap-2">
                      <input
                        id="rewind-anchor"
                        value={rewindAnchor}
                        onChange={(e) => setRewindAnchor(e.target.value)}
                        placeholder="e.g. 3 or st-auth"
                        className="flex-1 rounded-lg border border-slate-700 bg-slate-900 px-2 py-1.5 text-xs text-slate-200 focus:outline-none focus:ring-2 focus:ring-sky-500/40"
                      />
                      <button
                        type="button"
                        onClick={rewind}
                        disabled={busyAction !== null || !rewindAnchor.trim()}
                        className="rounded-lg border border-slate-700 bg-slate-900 px-3 py-1.5 text-xs text-slate-200 hover:border-sky-600 disabled:opacity-50"
                      >
                        Rewind
                      </button>
                    </div>
                    <p className="mt-1 text-[10px] text-slate-500">
                      Resets this subtask and every subtask after it to pending.
                    </p>
                  </div>
                  <div>
                    <label
                      htmlFor="redo-subtask"
                      className="mb-1 block text-xs font-medium text-slate-400"
                    >
                      Redo step (single subtask)
                    </label>
                    <div className="flex gap-2">
                      <input
                        id="redo-subtask"
                        value={redoSubtask}
                        onChange={(e) => setRedoSubtask(e.target.value)}
                        placeholder="subtask id or prefix"
                        className="flex-1 rounded-lg border border-slate-700 bg-slate-900 px-2 py-1.5 text-xs text-slate-200 focus:outline-none focus:ring-2 focus:ring-sky-500/40"
                      />
                      <button
                        type="button"
                        onClick={redo}
                        disabled={busyAction !== null || !redoSubtask.trim()}
                        className="rounded-lg border border-slate-700 bg-slate-900 px-3 py-1.5 text-xs text-slate-200 hover:border-sky-600 disabled:opacity-50"
                      >
                        Redo
                      </button>
                    </div>
                    <p className="mt-1 text-[10px] text-slate-500">
                      Resets only the named subtask; leaves prior steps intact.
                    </p>
                  </div>
                </div>
              </div>

              {/* Subtasks */}
              <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
                <h3 className="mb-3 text-xs font-medium uppercase tracking-wide text-slate-400">
                  Subtasks
                </h3>
                <div className="space-y-1">
                  {(selected.plan?.subtasks ?? []).map((st, i) => (
                    <div
                      key={st.id}
                      className="flex items-center justify-between rounded-lg border border-slate-700 bg-slate-900/50 px-3 py-2 text-sm"
                    >
                      <div className="flex items-center gap-3">
                        <span className="w-6 text-right text-xs text-slate-500">
                          {i + 1}
                        </span>
                        <span className="font-mono text-xs text-slate-500">
                          {st.id}
                        </span>
                        <span className="text-slate-200">{st.title}</span>
                      </div>
                      <span
                        className={`text-xs font-medium ${
                          st.status === 'completed'
                            ? 'text-green-400'
                            : st.status === 'in_progress'
                              ? 'text-sky-400'
                              : st.status === 'failed'
                                ? 'text-red-400'
                                : 'text-slate-500'
                        }`}
                      >
                        {st.status}
                      </span>
                    </div>
                  ))}
                  {(!selected.plan?.subtasks ||
                    selected.plan.subtasks.length === 0) && (
                    <p className="text-sm text-slate-500">No subtasks yet.</p>
                  )}
                </div>
              </div>

              {/* Step-run history */}
              <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
                <h3 className="mb-3 text-xs font-medium uppercase tracking-wide text-slate-400">
                  Step-run history ({runs.length})
                </h3>
                <div className="space-y-1">
                  {runs.length === 0 && (
                    <p className="text-sm text-slate-500">
                      No attempts recorded yet.
                    </p>
                  )}
                  {runs.map((r) => (
                    <div
                      key={r.run_id}
                      className="flex items-center justify-between gap-3 rounded-lg border border-slate-800 bg-slate-900/30 px-3 py-1.5 text-xs"
                    >
                      <div className="flex items-center gap-3">
                        <span className="font-mono text-slate-500">
                          #{r.attempt}
                        </span>
                        <span className="font-mono text-slate-400">
                          {r.subtask_id}
                        </span>
                        <span
                          className={`font-medium ${
                            r.status === 'completed'
                              ? 'text-green-400'
                              : r.status === 'in_progress'
                                ? 'text-sky-400'
                                : r.status === 'failed'
                                  ? 'text-red-400'
                                  : 'text-slate-500'
                          }`}
                        >
                          {r.status}
                        </span>
                      </div>
                      <div className="flex items-center gap-3 text-slate-500">
                        <span>session={r.session_id.slice(0, 10)}…</span>
                        <span>req={r.request_id.slice(0, 10)}…</span>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            </>
          ) : (
            <div className="rounded-2xl border border-dashed border-slate-700 p-12 text-center">
              <p className="text-slate-400">Select a plan on the left.</p>
            </div>
          )}
        </div>
      </div>

      {/* Delete confirm modal — replaces the native `confirm()` dialog so
          the prompt is themable and testable inside the app shell. */}
      {confirmDelete !== null && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
          role="dialog"
          aria-modal="true"
          aria-labelledby="delete-title"
        >
          <div className="w-full max-w-md rounded-2xl border border-slate-700 bg-slate-900 p-6 shadow-xl">
            <h2 id="delete-title" className="text-lg font-semibold text-white">
              Delete plan
            </h2>
            <p className="mt-2 text-sm text-slate-300">
              <code className="rounded bg-slate-800 px-1">{confirmDelete}</code> will be removed
              along with its step-run history. This can't be undone.
            </p>
            <div className="mt-5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirmDelete(null)}
                className="rounded-lg border border-slate-700 bg-slate-900 px-3 py-1.5 text-xs text-slate-200 hover:border-slate-600"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => performDelete(confirmDelete)}
                className="rounded-lg border border-red-700 bg-red-950/60 px-3 py-1.5 text-xs text-red-200 hover:bg-red-900/60"
                autoFocus
              >
                Delete
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
