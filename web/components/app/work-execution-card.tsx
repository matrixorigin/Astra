"use client";

import type {
  WorkBranchAttachmentV1,
  WorkBranchControlBasisV1,
  WorkExecutionSwitchOperationV1,
  WorkExecutionTargetPageV1,
  WorkExecutionViewV1,
} from "@astra/sdk";
import { ArrowRight, CircleAlert, Monitor, RefreshCw } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import {
  acquireWorkBranchControlAction,
  loadWorkExecutionAction,
  loadWorkExecutionTargetsAction,
  observeWorkBranchControlAction,
  observeWorkExecutionSwitchAction,
  retryWorkExecutionSwitchAction,
  switchWorkExecutionAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils/cn";

const OPERATION_POLL_DELAYS_MS = [350, 700, 1_200, 2_000, 3_000, 3_000] as const;

function executionStateLabel(execution: WorkExecutionViewV1): string {
  if (execution.state === "switching") return "Moving between devices";
  if (execution.state === "needs_attention") return "Needs attention";
  if (!execution.initialized) return "Ready to choose a provider";
  return execution.placement === "edge" ? "Running on Edge" : "Running on Server";
}

function operationStateLabel(operation: WorkExecutionSwitchOperationV1): string {
  if (operation.state === "switching") return "Checking both workspaces…";
  if (operation.state === "succeeded") return "Move confirmed";
  return "Move needs another try";
}

function actionErrorMessage(code: string): string {
  switch (code) {
    case "controller_attachment_required":
      return "Take control of this Work here before moving its execution.";
    case "execution_switch_conflict":
    case "execution_binding_fenced":
      return "This Work advanced elsewhere. Refresh its current execution before trying again.";
    case "source_workspace_dirty":
    case "workspace_revision_mismatch":
      return "Both devices must show the same clean Git revision before the move can start.";
    case "target_edge_unavailable":
      return "That Edge is no longer available. Refresh the target list and choose another.";
    default:
      return "The durable move was not confirmed. Its current state is still safe to check.";
  }
}

async function waitForOperation(
  workId: string,
  branchId: string,
  operationId: string,
  isCurrent: () => boolean,
  onUpdate: (operation: WorkExecutionSwitchOperationV1) => void,
): Promise<WorkExecutionSwitchOperationV1 | null> {
  for (let attempt = 0; attempt < OPERATION_POLL_DELAYS_MS.length; attempt += 1) {
    await new Promise((resolve) => window.setTimeout(resolve, OPERATION_POLL_DELAYS_MS[attempt]));
    if (!isCurrent()) return null;
    const result = await observeWorkExecutionSwitchAction({ workId, branchId, operationId });
    if (!isCurrent()) return null;
    if (!result.ok) throw new Error(result.code ?? "execution_switch_unavailable");
    onUpdate(result.operation);
    if (result.operation.state !== "switching") return result.operation;
  }
  return null;
}

export function WorkExecutionCard({
  workId,
  branchId,
  initialExecution,
  attachment,
  branchRevision,
  controlBasis,
}: {
  workId: string;
  branchId: string;
  initialExecution?: WorkExecutionViewV1 | null;
  attachment?: WorkBranchAttachmentV1 | null;
  branchRevision?: number;
  controlBasis?: WorkBranchControlBasisV1;
}) {
  const [execution, setExecution] = useState<WorkExecutionViewV1 | null>(initialExecution ?? null);
  const [targets, setTargets] = useState<WorkExecutionTargetPageV1 | null>(null);
  const [targetsOpen, setTargetsOpen] = useState(false);
  const [targetsLoading, setTargetsLoading] = useState(false);
  const [operation, setOperation] = useState<WorkExecutionSwitchOperationV1 | null>(null);
  const [controllerReady, setControllerReady] = useState(attachment?.mode === "controller");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const operationGeneration = useRef(0);
  const requestId = useRef<string | null>(null);
  const controlRequestId = useRef<string | null>(null);
  const mounted = useRef(true);
  const router = useRouter();

  const loadExecutionState = useCallback(
    async (generation: number, reportError = true): Promise<WorkExecutionViewV1 | null> => {
      try {
        const result = await loadWorkExecutionAction({ workId, branchId });
        if (!mounted.current || operationGeneration.current !== generation) return null;
        if (!result.ok) {
          if (reportError) setError(actionErrorMessage(result.code ?? "execution_unavailable"));
          return null;
        }
        setExecution(result.execution);
        return result.execution;
      } catch {
        if (reportError && mounted.current && operationGeneration.current === generation) {
          setError("The current execution could not be refreshed yet.");
        }
        return null;
      }
    },
    [branchId, workId],
  );

  const hydrateOperation = useCallback(
    async (currentExecution: WorkExecutionViewV1 | null | undefined, generation: number) => {
      const operationId = currentExecution?.operation_id;
      if (!operationId) {
        if (mounted.current && operationGeneration.current === generation) setOperation(null);
        return;
      }
      try {
        const result = await observeWorkExecutionSwitchAction({ workId, branchId, operationId });
        if (!mounted.current || operationGeneration.current !== generation) return;
        if (!result.ok) {
          setError(actionErrorMessage(result.code ?? "execution_switch_unavailable"));
          return;
        }
        setOperation(result.operation);
        if (result.operation.state !== "switching") {
          if (result.operation.state === "failed") {
            setError(actionErrorMessage(result.operation.failure_code ?? "execution_switch_failed"));
          }
          await loadExecutionState(generation, false);
          return;
        }
        const settled = await waitForOperation(
          workId,
          branchId,
          operationId,
          () => mounted.current && operationGeneration.current === generation,
          setOperation,
        );
        if (!mounted.current || operationGeneration.current !== generation) return;
        if (settled === null) {
          setError("The move is still recorded. Check again to see its durable result.");
        } else if (settled.state === "failed") {
          setError(actionErrorMessage(settled.failure_code ?? "execution_switch_failed"));
        }
        if (settled) await loadExecutionState(generation, false);
      } catch {
        if (mounted.current && operationGeneration.current === generation) {
          setError("The durable move could not be checked yet. Refresh to try again.");
        }
      }
    },
    [branchId, loadExecutionState, workId],
  );

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      operationGeneration.current += 1;
    };
  }, []);

  useEffect(() => {
    setExecution(initialExecution ?? null);
    setTargets(null);
    setTargetsOpen(false);
    setTargetsLoading(false);
    setOperation(null);
    setBusy(false);
    setRefreshing(false);
    setControllerReady(attachment?.mode === "controller");
    setError(null);
    requestId.current = null;
    controlRequestId.current = null;
    const generation = operationGeneration.current + 1;
    operationGeneration.current = generation;
    void hydrateOperation(initialExecution ?? null, generation);
  }, [attachment?.mode, branchId, hydrateOperation, initialExecution]);

  const refreshExecution = useCallback(async (actionGeneration?: number) => {
    const generation = actionGeneration ?? operationGeneration.current + 1;
    if (actionGeneration === undefined) {
      operationGeneration.current = generation;
      // A refresh supersedes any in-flight target read. Its stale response
      // must not leave the picker stuck in a loading state.
      setTargetsLoading(false);
      setTargets(null);
      setTargetsOpen(false);
    }
    setRefreshing(true);
    try {
      const loaded = await loadExecutionState(generation);
      if (loaded && mounted.current && operationGeneration.current === generation) {
        void hydrateOperation(loaded, generation);
      }
    } finally {
      if (mounted.current && operationGeneration.current === generation) setRefreshing(false);
    }
  }, [hydrateOperation, loadExecutionState]);

  async function openTargets() {
    if (targetsOpen) {
      setTargetsOpen(false);
      return;
    }
    setTargetsOpen(true);
    if (targets || targetsLoading) return;
    const generation = operationGeneration.current;
    setTargetsLoading(true);
    setError(null);
    try {
      const result = await loadWorkExecutionTargetsAction({ workId, branchId });
      if (!mounted.current || operationGeneration.current !== generation) return;
      if (!result.ok) {
        setError(actionErrorMessage(result.code ?? "execution_targets_unavailable"));
        return;
      }
      setTargets(result.page);
    } catch {
      if (mounted.current && operationGeneration.current === generation) {
        setError("Edge targets could not be loaded. Try again when the device is online.");
      }
    } finally {
      if (mounted.current && operationGeneration.current === generation) setTargetsLoading(false);
    }
  }

  async function ensureController(expectedGeneration = operationGeneration.current): Promise<boolean> {
    const isCurrent = () => mounted.current && operationGeneration.current === expectedGeneration;
    if (controllerReady) return true;
    if (!isCurrent()) return false;
    if (!attachment?.attachment_id || branchRevision === undefined || !controlBasis) {
      setError("Take control of this Work here before moving its execution.");
      return false;
    }
    const currentRequestId =
      controlRequestId.current ?? `web-execution-control:${crypto.randomUUID()}`;
    controlRequestId.current = currentRequestId;
    const result = await acquireWorkBranchControlAction({
      workId,
      branchId,
      attachmentId: attachment.attachment_id,
      requestId: currentRequestId,
      expectedBranchRevision: branchRevision,
      expectedControlBasis: controlBasis,
    });
    if (!isCurrent()) return false;
    if (!result.ok) {
      setError(actionErrorMessage(result.code ?? "controller_attachment_required"));
      if (!result.retryable) controlRequestId.current = null;
      return false;
    }
    if (result.operation.state === "pending") {
      const operation = await waitForControl(
        workId,
        branchId,
        result.operation.operation_id,
        isCurrent,
      );
      if (!operation || operation.state !== "succeeded") {
        if (!isCurrent()) return false;
        setError("Control was not confirmed. The Work remains safe to view on the other device.");
        return false;
      }
    } else if (result.operation.state !== "succeeded") {
      if (!isCurrent()) return false;
      setError("Control was not confirmed. The Work remains safe to view on the other device.");
      return false;
    }
    if (isCurrent()) {
      controlRequestId.current = null;
      setControllerReady(true);
    }
    return true;
  }

  async function waitForControl(
    currentWorkId: string,
    currentBranchId: string,
    operationId: string,
    isCurrent: () => boolean,
  ) {
    for (let attempt = 0; attempt < OPERATION_POLL_DELAYS_MS.length; attempt += 1) {
      await new Promise((resolve) => window.setTimeout(resolve, OPERATION_POLL_DELAYS_MS[attempt]));
      if (!isCurrent()) return null;
      const result = await observeWorkBranchControlAction({
        workId: currentWorkId,
        branchId: currentBranchId,
        operationId,
      });
      if (!isCurrent()) return null;
      if (!result.ok) throw new Error(result.code ?? "control_unavailable");
      if (result.operation.state !== "pending") return result.operation;
    }
    return null;
  }

  async function moveTo(executorId: string) {
    if (
      busy ||
      !execution ||
      execution.state !== "ready" ||
      execution.executor_id === executorId ||
      !attachment?.attachment_id
    ) {
      return;
    }
    const generation = operationGeneration.current + 1;
    operationGeneration.current = generation;
    setBusy(true);
    setError(null);
    try {
      if (!(await ensureController(generation))) return;
      if (!mounted.current || operationGeneration.current !== generation) return;
      const currentRequestId =
        requestId.current ?? `web-execution-switch:${crypto.randomUUID()}`;
      requestId.current = currentRequestId;
      const result = await switchWorkExecutionAction({
        workId,
        branchId,
        requestId: currentRequestId,
        attachmentId: attachment.attachment_id,
        expectedGeneration: execution.generation,
        targetExecutorId: executorId,
      });
      if (!mounted.current || operationGeneration.current !== generation) return;
      if (!result.ok) {
        setError(actionErrorMessage(result.code ?? "execution_switch_unavailable"));
        if (!result.retryable) requestId.current = null;
        return;
      }
      setOperation(result.operation);
      if (result.operation.state === "switching") {
        const settled = await waitForOperation(
          workId,
          branchId,
          result.operation.operation_id,
          () => mounted.current && operationGeneration.current === generation,
          setOperation,
        );
        if (!mounted.current || operationGeneration.current !== generation) return;
        if (settled === null) {
          setError("The move is still recorded. Check again to see its durable result.");
          return;
        }
        if (settled.state === "failed") {
          setError(actionErrorMessage(settled.failure_code ?? "execution_switch_failed"));
          return;
        }
      } else if (result.operation.state === "failed") {
        setError(actionErrorMessage(result.operation.failure_code ?? "execution_switch_failed"));
        return;
      }
      if (!mounted.current || operationGeneration.current !== generation) return;
      requestId.current = null;
      setTargetsOpen(false);
      await refreshExecution(generation);
      if (mounted.current) router.refresh();
    } catch {
      if (mounted.current && operationGeneration.current === generation) {
        setError("The move could not be confirmed. Its durable state is safe to check again.");
      }
    } finally {
      if (mounted.current && operationGeneration.current === generation) setBusy(false);
    }
  }

  async function retryMove() {
    if (
      busy ||
      !operation ||
      (operation.state !== "failed" && operation.state !== "switching") ||
      !attachment?.attachment_id
    )
      return;
    const generation = operationGeneration.current + 1;
    operationGeneration.current = generation;
    setBusy(true);
    setError(null);
    try {
      if (!(await ensureController(generation))) return;
      if (!mounted.current || operationGeneration.current !== generation) return;
      const result = await retryWorkExecutionSwitchAction({
        workId,
        branchId,
        operationId: operation.operation_id,
        attachmentId: attachment.attachment_id,
      });
      if (!mounted.current || operationGeneration.current !== generation) return;
      if (!result.ok) {
        setError(actionErrorMessage(result.code ?? "execution_switch_unavailable"));
        return;
      }
      setOperation(result.operation);
      if (result.operation.state === "switching") {
        const settled = await waitForOperation(
          workId,
          branchId,
          result.operation.operation_id,
          () => mounted.current && operationGeneration.current === generation,
          setOperation,
        );
        if (!mounted.current || operationGeneration.current !== generation) return;
        if (!settled || settled.state !== "succeeded") {
          setError("The retry is still recorded. Check again to see its durable result.");
          return;
        }
      } else if (result.operation.state !== "succeeded") {
        setError(actionErrorMessage(result.operation.failure_code ?? "execution_switch_failed"));
        return;
      }
      if (!mounted.current || operationGeneration.current !== generation) return;
      setOperation(null);
      setTargetsOpen(false);
      await refreshExecution(generation);
      if (mounted.current) router.refresh();
    } catch {
      if (mounted.current && operationGeneration.current === generation) {
        setError("The retry could not be confirmed. The previous durable result remains available.");
      }
    } finally {
      if (mounted.current && operationGeneration.current === generation) setBusy(false);
    }
  }

  const currentExecutor = execution?.executor_name || execution?.executor_id || "Server";
  const availableTargets = (targets?.targets ?? []).filter(
    (target) => target.connected && target.executor_id !== execution?.executor_id,
  );

  return (
    <Card className="space-y-4">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <Monitor className="size-4 text-accent" aria-hidden="true" />
            <p className="text-sm font-semibold text-text">Where this Work runs</p>
          </div>
          <p className="mt-1 text-xs leading-5 text-text-muted">
            {execution ? `${executionStateLabel(execution)} · ${currentExecutor}` : "Loading the current execution…"}
          </p>
          {execution ? (
            <p className="mt-1 text-[11px] tabular-nums text-text-muted">
              Durable generation {execution.generation}
              {execution.state === "needs_attention" && execution.failure_code
                ? ` · ${execution.failure_code}`
                : ""}
            </p>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <Button size="sm" variant="ghost" onClick={() => void refreshExecution()} disabled={refreshing || busy}>
            <RefreshCw className={cn("size-3.5", refreshing && "animate-spin")} aria-hidden="true" />
            Refresh
          </Button>
          {execution?.state === "ready" ? (
            <Button size="sm" variant="secondary" onClick={() => void openTargets()} disabled={busy || targetsLoading}>
              {targetsLoading ? "Loading…" : targetsOpen ? "Hide Edges" : "Move to another Edge"}
            </Button>
          ) : null}
        </div>
      </div>

      {operation ? (
        <div className="rounded-control border border-accent/20 bg-accent/5 px-3 py-3 text-xs text-text-secondary" role="status" aria-live="polite">
          <div className="flex items-center justify-between gap-3">
            <span>{operationStateLabel(operation)}</span>
            <span className="tabular-nums text-text-muted">Attempt {operation.attempt}</span>
          </div>
          {operation.state === "failed" || operation.state === "switching" ? (
            <div className="mt-3 flex flex-wrap items-center gap-2">
              {operation.state === "failed" ? (
                <span className="text-danger">
                  {operation.failure_code ?? "workspace checks did not match"}
                </span>
              ) : (
                <span className="text-text-muted">
                  If the other device disconnected, resume the recorded move here.
                </span>
              )}
              <Button size="sm" onClick={() => void retryMove()} disabled={busy}>
                {busy ? "Retrying…" : operation.state === "switching" ? "Resume move" : "Retry move"}
              </Button>
            </div>
          ) : null}
        </div>
      ) : null}

      {targetsOpen ? (
        <div className="border-t border-border/70 pt-3">
          {availableTargets.length > 0 ? (
            <ul className="grid gap-2 sm:grid-cols-2" aria-label="Connected Edge targets">
              {availableTargets.map((target) => (
                <li key={target.executor_id}>
                  <button
                    type="button"
                    className="flex w-full items-center justify-between gap-3 rounded-control border border-border bg-surface px-3 py-2 text-left transition hover:border-accent disabled:cursor-not-allowed disabled:opacity-60"
                    onClick={() => void moveTo(target.executor_id)}
                    disabled={busy}
                  >
                    <span className="min-w-0">
                      <span className="block truncate text-sm font-medium text-text">
                        {target.display_name || target.executor_id}
                      </span>
                      <span className="block truncate text-xs text-text-muted">
                        {target.hostname || target.executor_id}
                      </span>
                    </span>
                    <ArrowRight className="size-4 shrink-0 text-accent" aria-hidden="true" />
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <p className="text-xs leading-5 text-text-muted">
              No other connected Edge is available for this owner right now. Keep this page open and refresh when the other device comes online.
            </p>
          )}
        </div>
      ) : null}

      {error ? (
        <div className="flex items-start gap-2 rounded-control border border-warning/25 bg-warning/5 px-3 py-3 text-xs leading-5 text-text" role="alert">
          <CircleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden="true" />
          <span>{error}</span>
        </div>
      ) : null}

      {execution?.state === "needs_attention" && !error ? (
        <p className="text-xs leading-5 text-warning">
          The last move did not settle cleanly. Review its durable operation before trying another move.
        </p>
      ) : null}
    </Card>
  );
}
