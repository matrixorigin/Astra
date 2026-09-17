"use client";

import type {
  WorkPatchArtifactContent,
  WorkPatchArtifactPageV1,
  WorkPatchArtifactV1,
  WorkPatchMaterializationOperationV2,
  WorkPatchMaterializationPageV2,
  WorkPatchCommitOperationV1,
  WorkPatchCommitPageV1,
} from "@astra/sdk";
import { ChevronDown, ChevronRight, FileDiff, LoaderCircle } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  abortWorkPatchMaterializationAction,
  exportWorkPatchArtifactAction,
  loadWorkPatchArtifactsAction,
  loadWorkPatchContentAction,
  materializeWorkPatchAction,
  observeWorkPatchMaterializationAction,
  abortWorkPatchCommitAction,
  commitWorkPatchAction,
  observeWorkPatchCommitAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { UnifiedDiffView } from "@/components/app/unified-diff-view";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";

const DATE_FORMATTER = new Intl.DateTimeFormat("en", {
  dateStyle: "medium",
  timeStyle: "short",
  timeZone: "UTC",
});

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function shortRevision(revision: string): string {
  return revision.slice("sha256:".length, "sha256:".length + 10);
}

function operationStatus(operation: WorkPatchMaterializationOperationV2): string {
  if (operation.state === "succeeded") return "Applied and verified";
  if (operation.state === "conflict") {
    return "Main result changed, so this exact patch was not applied.";
  }
  if (operation.state === "failed") {
    return "Changes could not be applied safely.";
  }
  if (operation.state === "aborted") return "Application stopped";
  if (operation.phase === "applying") return "Applying exact changes…";
  if (operation.phase === "reconciling") return "Checking the applied result…";
  if (operation.phase === "verifying") return "Running relevant checks…";
  return "Waiting to apply…";
}

function operationSummary(operation: WorkPatchMaterializationOperationV2): string {
  if (operation.state === "succeeded") return "Applied";
  if (operation.state === "conflict") return "Conflict";
  if (operation.state === "failed") return "Failed";
  if (operation.state === "aborted") return "Stopped";
  if (operation.phase === "verifying") return "Verifying…";
  return "Applying…";
}

function latestMaterializationsByPatch(
  page: WorkPatchMaterializationPageV2 | null | undefined,
): Record<string, WorkPatchMaterializationOperationV2> {
  const result: Record<string, WorkPatchMaterializationOperationV2> = {};
  for (const operation of page?.operations ?? []) {
    result[operation.patch_artifact_id] ??= operation;
  }
  return result;
}

function latestCommitsByPatch(
  page: WorkPatchCommitPageV1 | null | undefined,
): Record<string, WorkPatchCommitOperationV1> {
  const result: Record<string, WorkPatchCommitOperationV1> = {};
  for (const operation of page?.operations ?? []) {
    result[operation.patch_artifact_id] ??= operation;
  }
  return result;
}

function commitStatus(operation: WorkPatchCommitOperationV1): string {
  if (operation.state === "succeeded") {
    return operation.index_reconciled
      ? `Committed as ${operation.commit_sha?.slice(0, 10)}`
      : `Committed as ${operation.commit_sha?.slice(0, 10)}; refresh the local index before editing.`;
  }
  if (operation.state === "conflict") {
    return operation.commit_sha
      ? "The commit was created, but Main result changed before Astra could publish its exact state."
      : "The reviewed result changed before the commit was created.";
  }
  if (operation.state === "failed") return "The commit could not be created safely.";
  if (operation.state === "aborted") return "Commit stopped";
  if (operation.phase === "reconciling") return "Checking whether the commit was created…";
  if (operation.phase === "committing") return "Creating the exact reviewed commit…";
  return "Waiting to commit…";
}

function commitSummary(operation: WorkPatchCommitOperationV1): string {
  if (operation.state === "succeeded") return "Committed";
  if (operation.state === "conflict") return "Commit conflict";
  if (operation.state === "failed") return "Commit failed";
  if (operation.state === "aborted") return "Commit stopped";
  return "Committing…";
}

export function WorkPatchReviewCard({
  workId,
  branchId,
  initial,
  initialMaterializations,
  initialCommits,
  exportBasis,
  materializeTarget,
  commitTarget,
}: {
  workId: string;
  branchId: string;
  initial: WorkPatchArtifactPageV1 | null;
  initialMaterializations?: WorkPatchMaterializationPageV2 | null;
  initialCommits?: WorkPatchCommitPageV1 | null;
  exportBasis: { branchRevision: number; graphRevision: number };
  materializeTarget?: {
    branchId: string;
    label: string;
    branchRevision: number;
    graphRevision: number;
  };
  commitTarget?: {
    branchId: string;
    label: string;
    branchRevision: number;
    graphRevision: number;
  };
}) {
  const [artifacts, setArtifacts] = useState(initial?.artifacts ?? []);
  const [nextCursor, setNextCursor] = useState(initial?.next_cursor ?? null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [contentById, setContentById] = useState<
    Record<string, WorkPatchArtifactContent>
  >({});
  const [loadingId, setLoadingId] = useState<string | null>(null);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [confirmingId, setConfirmingId] = useState<string | null>(null);
  const [materializingId, setMaterializingId] = useState<string | null>(null);
  const [stopping, setStopping] = useState(false);
  const [confirmingCommitId, setConfirmingCommitId] = useState<string | null>(null);
  const [commitMessage, setCommitMessage] = useState("Apply reviewed changes");
  const [committingId, setCommittingId] = useState<string | null>(null);
  const [stoppingCommit, setStoppingCommit] = useState(false);
  const [operationsByPatch, setOperationsByPatch] = useState(() =>
    latestMaterializationsByPatch(initialMaterializations),
  );
  const exportRequestId = useRef<string | null>(null);
  const materializationRequestIds = useRef(new Map<string, string>());
  const commitRequestIds = useRef(new Map<string, string>());
  const [commitsByPatch, setCommitsByPatch] = useState(() =>
    latestCommitsByPatch(initialCommits),
  );
  const [error, setError] = useState<string | null>(
    initial === null ? "Changes are temporarily unavailable." : null,
  );
  const [materializationError, setMaterializationError] = useState<string | null>(
    initialMaterializations === null && materializeTarget
      ? "Application history is temporarily unavailable."
      : null,
  );
  const [commitError, setCommitError] = useState<string | null>(
    initialCommits === null ? "Commit history is temporarily unavailable." : null,
  );

  const newest = artifacts[0];
  const canExport =
    initial !== null &&
    (newest === undefined ||
      newest.source_branch_revision !== exportBasis.branchRevision ||
      newest.source_graph_revision !== exportBasis.graphRevision);
  const pendingOperation = Object.values(operationsByPatch).find(
    (candidate) => candidate.state === "pending",
  );
  const pendingOperationId = pendingOperation?.operation_id ?? null;
  const pendingTargetBranchId = pendingOperation?.target_branch_id ?? null;
  const pendingCommit = Object.values(commitsByPatch).find(
    (candidate) => candidate.state === "pending",
  );
  const pendingCommitId = pendingCommit?.operation_id ?? null;
  const pendingCommitTargetBranchId = pendingCommit?.target_branch_id ?? null;

  useEffect(() => {
    if (!pendingOperationId || !pendingTargetBranchId) return;
    const operationId = pendingOperationId;
    const targetBranchId = pendingTargetBranchId;
    let cancelled = false;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    let finishDelay: (() => void) | undefined;

    async function observeUntilTerminal() {
      let attempt = 0;
      while (!cancelled) {
        await new Promise<void>((resolve) => {
          finishDelay = resolve;
          timeout = setTimeout(() => {
            finishDelay = undefined;
            resolve();
          }, Math.min(500 * 2 ** attempt, 4_000));
        });
        if (cancelled) return;
        try {
          const result = await observeWorkPatchMaterializationAction({
            workId,
            targetBranchId,
            operationId,
          });
          if (cancelled) return;
          if (!result.ok) {
            if (!result.retryable) {
              setMaterializationError("Application progress is no longer available.");
              return;
            }
            setMaterializationError(
              "Application progress is temporarily unavailable; tracking will continue.",
            );
            attempt += 1;
            continue;
          }
          setMaterializationError(null);
          setOperationsByPatch((current) => ({
            ...current,
            [result.operation.patch_artifact_id]: result.operation,
          }));
          if (result.operation.state !== "pending") return;
          attempt += 1;
        } catch {
          if (cancelled) return;
          setMaterializationError(
            "Application progress is temporarily unavailable; tracking will continue.",
          );
          attempt += 1;
        }
      }
    }

    void observeUntilTerminal();
    return () => {
      cancelled = true;
      if (timeout) clearTimeout(timeout);
      finishDelay?.();
    };
  }, [pendingOperationId, pendingTargetBranchId, workId]);

  useEffect(() => {
    if (!pendingCommitId || !pendingCommitTargetBranchId) return;
    const operationId = pendingCommitId;
    const targetBranchId = pendingCommitTargetBranchId;
    let cancelled = false;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    let finishDelay: (() => void) | undefined;

    async function observeUntilTerminal() {
      let attempt = 0;
      while (!cancelled) {
        await new Promise<void>((resolve) => {
          finishDelay = resolve;
          timeout = setTimeout(() => {
            finishDelay = undefined;
            resolve();
          }, Math.min(500 * 2 ** attempt, 4_000));
        });
        if (cancelled) return;
        try {
          const result = await observeWorkPatchCommitAction({
            workId,
            targetBranchId,
            operationId,
          });
          if (cancelled) return;
          if (!result.ok) {
            setCommitError(
              result.retryable
                ? "Commit progress is temporarily unavailable; tracking will continue."
                : "Commit progress is no longer available.",
            );
            if (!result.retryable) return;
            attempt += 1;
            continue;
          }
          setCommitError(null);
          setCommitsByPatch((current) => ({
            ...current,
            [result.operation.patch_artifact_id]: result.operation,
          }));
          if (result.operation.state !== "pending") return;
          attempt += 1;
        } catch {
          if (cancelled) return;
          setCommitError(
            "Commit progress is temporarily unavailable; tracking will continue.",
          );
          attempt += 1;
        }
      }
    }

    void observeUntilTerminal();
    return () => {
      cancelled = true;
      if (timeout) clearTimeout(timeout);
      finishDelay?.();
    };
  }, [pendingCommitId, pendingCommitTargetBranchId, workId]);

  async function exportCurrent() {
    if (!canExport || exporting) return;
    setExporting(true);
    setError(null);
    exportRequestId.current ??= `web-patch-export:${crypto.randomUUID()}`;
    try {
      const result = await exportWorkPatchArtifactAction({
        workId,
        branchId,
        requestId: exportRequestId.current,
        expectedBranchRevision: exportBasis.branchRevision,
        expectedGraphRevision: exportBasis.graphRevision,
      });
      if (!result.ok) {
        if (result.code === "work_patch_export_has_no_changes") {
          setError("The current workspace has no changes to prepare for review.");
        } else if (result.code === "work_patch_export_basis_conflict") {
          setError("The result changed before review preparation. Refresh to use its latest basis.");
        } else {
          setError(
            result.retryable
              ? "Review preparation did not complete. You can safely try again."
              : "The current result cannot be prepared for review.",
          );
        }
        return;
      }
      exportRequestId.current = null;
      setArtifacts((current) => [
        result.artifact,
        ...current.filter(
          (artifact) => artifact.patch_artifact_id !== result.artifact.patch_artifact_id,
        ),
      ]);
    } catch {
      setError("Review preparation did not complete. You can safely try again.");
    } finally {
      setExporting(false);
    }
  }

  async function toggleArtifact(artifact: WorkPatchArtifactV1) {
    if (selectedId === artifact.patch_artifact_id) {
      setSelectedId(null);
      return;
    }
    setSelectedId(artifact.patch_artifact_id);
    setError(null);
    if (contentById[artifact.patch_artifact_id]) return;
    setLoadingId(artifact.patch_artifact_id);
    try {
      const result = await loadWorkPatchContentAction({
        workId,
        branchId,
        patchArtifactId: artifact.patch_artifact_id,
      });
      if (!result.ok) {
        setError(
          result.retryable
            ? "Changes could not be loaded. You can safely try again."
            : "These changes are no longer available for review.",
        );
        return;
      }
      if (
        result.content.hash !== artifact.payload_hash ||
        result.content.bytes !== artifact.payload_bytes
      ) {
        setError("Change content does not match its immutable review record.");
        return;
      }
      setContentById((current) => ({
        ...current,
        [artifact.patch_artifact_id]: result.content,
      }));
    } catch {
      setError("Changes could not be loaded. You can safely try again.");
    } finally {
      setLoadingId(null);
    }
  }

  async function loadOlder() {
    if (!nextCursor || loadingOlder) return;
    setLoadingOlder(true);
    setError(null);
    try {
      const result = await loadWorkPatchArtifactsAction({
        workId,
        branchId,
        before: nextCursor,
      });
      if (!result.ok) {
        setError(
          result.retryable
            ? "Earlier changes could not be loaded. You can safely try again."
            : "Earlier changes are no longer available.",
        );
        return;
      }
      const existing = new Set(artifacts.map((artifact) => artifact.patch_artifact_id));
      if (result.page.artifacts.some((artifact) => existing.has(artifact.patch_artifact_id))) {
        setError("The change history moved unexpectedly. Refresh to reconcile it.");
        return;
      }
      setArtifacts((current) => [...current, ...result.page.artifacts]);
      setNextCursor(result.page.next_cursor);
    } catch {
      setError("Earlier changes could not be loaded. You can safely try again.");
    } finally {
      setLoadingOlder(false);
    }
  }

  async function materialize(artifact: WorkPatchArtifactV1) {
    if (!materializeTarget || materializingId || pendingOperation) return;
    setMaterializingId(artifact.patch_artifact_id);
    setMaterializationError(null);
    const requestId =
      materializationRequestIds.current.get(artifact.patch_artifact_id) ??
      `web-patch-materialization:${crypto.randomUUID()}`;
    materializationRequestIds.current.set(artifact.patch_artifact_id, requestId);
    try {
      const result = await materializeWorkPatchAction({
        workId,
        targetBranchId: materializeTarget.branchId,
        patchArtifactId: artifact.patch_artifact_id,
        requestId,
        expectedTargetBranchRevision: materializeTarget.branchRevision,
        expectedTargetGraphRevision: materializeTarget.graphRevision,
      });
      if (!result.ok) {
        setMaterializationError(
          result.code === "work_patch_materialization_conflict" ||
            result.code === "work_patch_materialization_busy"
            ? "Main result changed before application. Refresh and prepare a new exact patch."
            : result.retryable
              ? "Application did not start. You can safely try again."
              : "These changes cannot be applied safely.",
        );
        return;
      }
      materializationRequestIds.current.delete(artifact.patch_artifact_id);
      setConfirmingId(null);
      setOperationsByPatch((current) => ({
        ...current,
        [result.operation.patch_artifact_id]: result.operation,
      }));
    } catch {
      setMaterializationError("Application did not start. You can safely try again.");
    } finally {
      setMaterializingId(null);
    }
  }

  async function stopMaterialization(
    operation: WorkPatchMaterializationOperationV2,
  ) {
    if (operation.state !== "pending" || stopping) return;
    setStopping(true);
    setMaterializationError(null);
    const identity = {
      workId,
      targetBranchId: operation.target_branch_id,
      operationId: operation.operation_id,
    };
    try {
      await abortWorkPatchMaterializationAction(identity);
      const observed = await observeWorkPatchMaterializationAction(identity);
      if (observed.ok) {
        setOperationsByPatch((current) => ({
          ...current,
          [observed.operation.patch_artifact_id]: observed.operation,
        }));
      } else {
        setMaterializationError("The stop result is uncertain. Refresh to reconcile it.");
      }
    } catch {
      setMaterializationError("The stop result is uncertain. Refresh to reconcile it.");
    } finally {
      setStopping(false);
    }
  }

  async function commitPatch(
    artifact: WorkPatchArtifactV1,
    materialization: WorkPatchMaterializationOperationV2 | null,
  ) {
    if (!commitTarget || committingId || pendingCommit || commitMessage.trim().length === 0) {
      return;
    }
    const targetWasMaterialized =
      materialization?.state === "succeeded" &&
      materialization.target_branch_id === commitTarget.branchId;
    if (branchId !== commitTarget.branchId && !targetWasMaterialized) return;
    const expectedTargetBranchRevision = targetWasMaterialized
      ? materialization.target_branch_revision + 1
      : commitTarget.branchRevision;
    const expectedTargetGraphRevision = targetWasMaterialized
      ? materialization.target_graph_revision
      : commitTarget.graphRevision;
    setCommittingId(artifact.patch_artifact_id);
    setCommitError(null);
    const requestId =
      commitRequestIds.current.get(artifact.patch_artifact_id) ??
      `web-patch-commit:${crypto.randomUUID()}`;
    commitRequestIds.current.set(artifact.patch_artifact_id, requestId);
    try {
      const result = await commitWorkPatchAction({
        workId,
        targetBranchId: commitTarget.branchId,
        patchArtifactId: artifact.patch_artifact_id,
        requestId,
        expectedTargetBranchRevision,
        expectedTargetGraphRevision,
        message: commitMessage,
      });
      if (!result.ok) {
        setCommitError(
          result.code === "work_patch_commit_conflict" ||
            result.code === "work_patch_commit_busy"
            ? `${commitTarget.label} changed before commit. Refresh and review its current result.`
            : result.retryable
              ? "Commit did not start. You can safely try again."
              : "This reviewed result cannot be committed safely.",
        );
        return;
      }
      commitRequestIds.current.delete(artifact.patch_artifact_id);
      setConfirmingCommitId(null);
      setCommitsByPatch((current) => ({
        ...current,
        [result.operation.patch_artifact_id]: result.operation,
      }));
    } catch {
      setCommitError("Commit did not start. You can safely try again.");
    } finally {
      setCommittingId(null);
    }
  }

  async function stopCommit(operation: WorkPatchCommitOperationV1) {
    if (
      operation.state !== "pending" ||
      operation.phase !== "awaiting_dispatch" ||
      stoppingCommit
    ) {
      return;
    }
    setStoppingCommit(true);
    setCommitError(null);
    const identity = {
      workId,
      targetBranchId: operation.target_branch_id,
      operationId: operation.operation_id,
    };
    try {
      await abortWorkPatchCommitAction(identity);
      const observed = await observeWorkPatchCommitAction(identity);
      if (observed.ok) {
        setCommitsByPatch((current) => ({
          ...current,
          [observed.operation.patch_artifact_id]: observed.operation,
        }));
      } else {
        setCommitError("The stop result is uncertain. Refresh to reconcile it.");
      }
    } catch {
      setCommitError("The stop result is uncertain. Refresh to reconcile it.");
    } finally {
      setStoppingCommit(false);
    }
  }

  return (
    <Card className="overflow-hidden p-0">
      <div className="flex items-start gap-3 px-5 py-4">
        <div className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
          <FileDiff className="size-4" />
        </div>
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-semibold text-text">Results</h2>
          <p className="mt-1 text-sm leading-6 text-text-secondary">
            Exact workspace changes, exported from a pinned result revision.
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {artifacts.length > 0 ? (
            <span className="rounded-full bg-surface-muted px-2 py-0.5 text-xs tabular-nums text-text-muted">
              {artifacts.length}{nextCursor ? "+" : ""}
            </span>
          ) : null}
          {canExport ? (
            <Button size="sm" variant="secondary" disabled={exporting} onClick={() => void exportCurrent()}>
              {exporting ? "Preparing…" : "Prepare review"}
            </Button>
          ) : null}
        </div>
      </div>

      {error ? (
        <p role="alert" className="border-t border-danger/20 bg-danger/5 px-5 py-3 text-sm text-danger">
          {error}
        </p>
      ) : null}

      {materializationError ? (
        <p role="alert" className="border-t border-danger/20 bg-danger/5 px-5 py-3 text-sm text-danger">
          {materializationError}
        </p>
      ) : null}

      {commitError ? (
        <p role="alert" className="border-t border-danger/20 bg-danger/5 px-5 py-3 text-sm text-danger">
          {commitError}
        </p>
      ) : null}

      {artifacts.length > 0 ? (
        <div className="divide-y divide-border/70 border-t border-border/70">
          {artifacts.map((artifact, index) => {
            const selected = selectedId === artifact.patch_artifact_id;
            const loading = loadingId === artifact.patch_artifact_id;
            const content = contentById[artifact.patch_artifact_id];
            const artifactOperation =
              operationsByPatch[artifact.patch_artifact_id] ?? null;
            const artifactCommit = commitsByPatch[artifact.patch_artifact_id] ?? null;
            const commitEligible =
              commitTarget !== undefined &&
              (branchId === commitTarget.branchId ||
                (artifactOperation?.state === "succeeded" &&
                  artifactOperation.target_branch_id === commitTarget.branchId));
            return (
              <section key={artifact.patch_artifact_id}>
                <button
                  type="button"
                  className="flex w-full items-center gap-3 px-5 py-3 text-left hover:bg-surface-muted/60"
                  aria-expanded={selected}
                  onClick={() => void toggleArtifact(artifact)}
                >
                  {loading ? (
                    <LoaderCircle className="size-4 shrink-0 animate-spin text-accent" />
                  ) : selected ? (
                    <ChevronDown className="size-4 shrink-0 text-text-muted" />
                  ) : (
                    <ChevronRight className="size-4 shrink-0 text-text-muted" />
                  )}
                  <span className="min-w-0 flex-1">
                    <span className="block text-sm font-medium text-text">
                      {index === 0 ? "Latest changes" : "Earlier changes"}
                    </span>
                    <span className="mt-0.5 block text-xs tabular-nums text-text-muted">
                      {formatBytes(artifact.payload_bytes)} · {shortRevision(artifact.base_subject_revision)} → {shortRevision(artifact.result_subject_revision)}
                    </span>
                  </span>
                  {artifactOperation && !artifactCommit ? (
                    <span className="shrink-0 text-xs font-medium text-text-secondary">
                      {operationSummary(artifactOperation)}
                    </span>
                  ) : null}
                  {artifactCommit ? (
                    <span className="shrink-0 text-xs font-medium text-text-secondary">
                      {commitSummary(artifactCommit)}
                    </span>
                  ) : null}
                  <time className="shrink-0 text-xs text-text-muted" dateTime={artifact.created_at}>
                    {DATE_FORMATTER.format(new Date(artifact.created_at))}
                  </time>
                </button>
                {selected && content ? (
                  <>
                    <UnifiedDiffView data={content.data} />
                    {materializeTarget ? (
                      <div className="border-t border-border/70 px-5 py-3">
                        {artifactOperation ? (
                          <div className="flex items-center justify-between gap-3">
                            <p
                              role="status"
                              className={`text-sm ${
                                artifactOperation.state === "succeeded"
                                  ? "text-success"
                                  : artifactOperation.state === "conflict" ||
                                      artifactOperation.state === "failed"
                                    ? "text-danger"
                                    : "text-text-secondary"
                              }`}
                            >
                              {operationStatus(artifactOperation)}
                            </p>
                            {artifactOperation.state === "pending" ? (
                              <Button
                                size="sm"
                                variant="ghost"
                                disabled={stopping}
                                onClick={() => void stopMaterialization(artifactOperation)}
                              >
                                {stopping ? "Stopping…" : "Stop"}
                              </Button>
                            ) : null}
                          </div>
                        ) : confirmingId === artifact.patch_artifact_id ? (
                          <div className="flex flex-wrap items-center justify-between gap-3">
                            <p className="max-w-2xl text-sm leading-5 text-text-secondary">
                              Apply this exact patch only if {materializeTarget.label} still has
                              the expected base. Relevant checks run automatically.
                            </p>
                            <div className="flex items-center gap-2">
                              <Button
                                size="sm"
                                variant="ghost"
                                disabled={materializingId !== null}
                                onClick={() => setConfirmingId(null)}
                              >
                                Cancel
                              </Button>
                              <Button
                                size="sm"
                                disabled={materializingId !== null}
                                onClick={() => void materialize(artifact)}
                              >
                                {materializingId === artifact.patch_artifact_id
                                  ? "Starting…"
                                  : "Apply changes"}
                              </Button>
                            </div>
                          </div>
                        ) : (
                          <Button
                            size="sm"
                            variant="secondary"
                            disabled={pendingOperation !== undefined}
                            onClick={() => setConfirmingId(artifact.patch_artifact_id)}
                          >
                            {pendingOperation
                              ? "Application in progress"
                              : `Bring to ${materializeTarget.label}`}
                          </Button>
                        )}
                      </div>
                    ) : null}
                    {commitEligible && commitTarget ? (
                      <div className="border-t border-border/70 px-5 py-3">
                        {artifactCommit ? (
                          <div className="flex items-center justify-between gap-3">
                            <p
                              role="status"
                              className={`text-sm ${
                                artifactCommit.state === "succeeded"
                                  ? "text-success"
                                  : artifactCommit.state === "conflict" ||
                                      artifactCommit.state === "failed"
                                    ? "text-danger"
                                    : "text-text-secondary"
                              }`}
                            >
                              {commitStatus(artifactCommit)}
                            </p>
                            {artifactCommit.state === "pending" &&
                            artifactCommit.phase === "awaiting_dispatch" ? (
                              <Button
                                size="sm"
                                variant="ghost"
                                disabled={stoppingCommit}
                                onClick={() => void stopCommit(artifactCommit)}
                              >
                                {stoppingCommit ? "Stopping…" : "Stop"}
                              </Button>
                            ) : null}
                          </div>
                        ) : confirmingCommitId === artifact.patch_artifact_id ? (
                          <div className="space-y-3">
                            <p className="text-sm leading-5 text-text-secondary">
                              Create one Git commit from this exact reviewed patch in {commitTarget.label}.
                              Files added or changed after review are excluded, and the commit starts only after confirmation.
                            </p>
                            <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
                              <Input
                                aria-label="Commit message"
                                value={commitMessage}
                                maxLength={4096}
                                disabled={committingId !== null}
                                onChange={(event) => setCommitMessage(event.target.value)}
                              />
                              <div className="flex shrink-0 items-center gap-2">
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  disabled={committingId !== null}
                                  onClick={() => setConfirmingCommitId(null)}
                                >
                                  Cancel
                                </Button>
                                <Button
                                  size="sm"
                                  disabled={
                                    committingId !== null || commitMessage.trim().length === 0
                                  }
                                  onClick={() => void commitPatch(artifact, artifactOperation)}
                                >
                                  {committingId === artifact.patch_artifact_id
                                    ? "Starting…"
                                    : "Create commit"}
                                </Button>
                              </div>
                            </div>
                          </div>
                        ) : (
                          <Button
                            size="sm"
                            variant="secondary"
                            disabled={pendingCommit !== undefined}
                            onClick={() => setConfirmingCommitId(artifact.patch_artifact_id)}
                          >
                            {pendingCommit ? "Commit in progress" : "Commit reviewed changes"}
                          </Button>
                        )}
                      </div>
                    ) : null}
                  </>
                ) : null}
              </section>
            );
          })}
          {nextCursor ? (
            <div className="px-5 py-3">
              <Button size="sm" variant="ghost" disabled={loadingOlder} onClick={() => void loadOlder()}>
                {loadingOlder ? "Loading…" : "Show earlier exports"}
              </Button>
            </div>
          ) : null}
        </div>
      ) : null}
    </Card>
  );
}
