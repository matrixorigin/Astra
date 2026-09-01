"use client";

import type {
  WorkCriteriaProposalDetailV1,
  WorkCriteriaProposalMemberV1,
  WorkCriteriaProposalSummaryV1,
  WorkBranchAttachmentV1,
  WorkArchivedBranchEntryV1,
  WorkArchivedBranchPageV1,
  WorkBranchCatalogEntryV1,
  WorkBranchCatalogV1,
  WorkBranchCreationOperationV1,
  WorkBranchDeletionOperationV1,
  WorkBranchComparisonReportV2,
  WorkCriterionV1,
  WorkObservationFactCodeV1,
  WorkTranscriptPageV1,
  WorkPatchArtifactPageV1,
  WorkPatchMaterializationPageV2,
  WorkPatchCommitPageV1,
} from "@astra/sdk";
import {
  Archive,
  ArchiveRestore,
  Check,
  ChevronDown,
  ChevronRight,
  CircleDot,
  ClipboardCheck,
  GitBranch,
  GitFork,
  ListChecks,
  Trash2,
  X,
} from "lucide-react";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  abortWorkBranchCreationAction,
  changeWorkBranchRetentionAction,
  compareWorkBranchesAction,
  createWorkBranchAction,
  deleteWorkBranchAction,
  loadArchivedWorkBranchesAction,
  loadCriteriaProposalAction,
  observeWorkBranchCreationAction,
  observeWorkBranchDeletionAction,
  resolveCriteriaProposalAction,
  selectWorkDeliveryAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { useToast } from "@/components/ui/toast";
import { WorkTurnComposer } from "@/components/app/work-turn-composer";
import { WorkActivityCard } from "@/components/app/work-activity-card";
import { WorkTranscriptCard } from "@/components/app/work-transcript-card";
import { WorkPatchReviewCard } from "@/components/app/work-patch-review-card";
import { WorkTaskGraph } from "@/components/app/work-task-graph";
import type { WorkOverviewSnapshot } from "@/lib/work-overview";
import type { WorkActionError } from "@/lib/work-action-error";
import { cn } from "@/lib/utils/cn";

const deliveryLabels: Record<
  WorkObservationFactCodeV1,
  { label: string; detail: string; tone: string }
> = {
  criteria_not_accepted: {
    label: "Criteria open",
    detail: "Done-when criteria still need to be accepted.",
    tone: "bg-warning",
  },
  branch_basis_out_of_date: {
    label: "Needs review",
    detail: "This branch is behind the current goal or Done-when criteria.",
    tone: "bg-warning",
  },
  subject_unavailable: {
    label: "Result pending",
    detail: "There is no current result revision to verify yet.",
    tone: "bg-warning",
  },
  verification_required: {
    label: "Checks needed",
    detail: "Current results still need fresh checks.",
    tone: "bg-accent",
  },
  ready_for_review: {
    label: "Ready for review",
    detail: "Accepted criteria have current supporting evidence.",
    tone: "bg-success",
  },
};

const FORK_POLL_DELAYS_MS = [500, 1_000, 2_000, 4_000, 4_000, 4_000] as const;
const DELETION_POLL_DELAYS_MS = [500, 1_000, 2_000, 4_000, 4_000, 4_000] as const;
const ARCHIVE_DATE_FORMATTER = new Intl.DateTimeFormat("en", {
  dateStyle: "medium",
  timeZone: "UTC",
});

function deletionProgressLabel(
  phase: WorkBranchDeletionOperationV1["phase"],
): string {
  switch (phase) {
    case "fence":
      return "Stopping active turns…";
    case "session_cleanup":
      return "Removing session history…";
    case "lineage_gc":
      return "Checking shared history…";
    case "branch_cleanup":
      return "Removing approach…";
    case "complete":
      return "Deletion complete";
    default:
      return "Deleting approach…";
  }
}

export function WorkOverviewPage({
  initial,
  attachment,
  transcript,
  branchCatalog,
  selectedBranch,
  archivedBranches,
  patchArtifacts,
  patchMaterializations,
  patchCommits,
}: {
  initial: WorkOverviewSnapshot;
  attachment?: WorkBranchAttachmentV1 | null;
  transcript?: WorkTranscriptPageV1 | null;
  branchCatalog: WorkBranchCatalogV1;
  selectedBranch: WorkBranchCatalogEntryV1;
  archivedBranches?: WorkArchivedBranchPageV1 | null;
  patchArtifacts?: WorkPatchArtifactPageV1 | null;
  patchMaterializations?: WorkPatchMaterializationPageV2 | null;
  patchCommits?: WorkPatchCommitPageV1 | null;
}) {
  const [snapshot, setSnapshot] = useState(initial);
  const [turnActive, setTurnActive] = useState(false);
  const [expandedProposalId, setExpandedProposalId] = useState<string | null>(
    null,
  );
  const [proposalDetails, setProposalDetails] = useState<
    Record<string, WorkCriteriaProposalDetailV1>
  >({});
  const [loadingProposalId, setLoadingProposalId] = useState<string | null>(null);
  const [resolvingProposalId, setResolvingProposalId] = useState<string | null>(
    null,
  );
  const [reviewError, setReviewError] = useState<string | null>(null);
  const actionIds = useRef(new Map<string, string>());
  const forkRequestId = useRef<string | null>(null);
  const forkPollAttempt = useRef(0);
  const [forkOperation, setForkOperation] =
    useState<WorkBranchCreationOperationV1 | null>(null);
  const [forkStarting, setForkStarting] = useState(false);
  const [forkStopping, setForkStopping] = useState(false);
  const [forkError, setForkError] = useState<string | null>(null);
  const [comparison, setComparison] =
    useState<WorkBranchComparisonReportV2 | null>(null);
  const [comparisonLoading, setComparisonLoading] = useState(false);
  const [comparisonError, setComparisonError] = useState<string | null>(null);
  const [selectionError, setSelectionError] = useState<string | null>(null);
  const [selectionPending, setSelectionPending] = useState(false);
  const selectionRequestId = useRef<string | null>(null);
  const [retentionPending, setRetentionPending] = useState<string | null>(null);
  const [retentionError, setRetentionError] = useState<string | null>(null);
  const [archivePage, setArchivePage] = useState(archivedBranches ?? null);
  const [archivePageLoading, setArchivePageLoading] = useState(false);
  const [deletionConfirmation, setDeletionConfirmation] = useState<string | null>(null);
  const [deletionOperation, setDeletionOperation] =
    useState<WorkBranchDeletionOperationV1 | null>(null);
  const deletionPollAttempt = useRef(0);
  const comparisonGeneration = useRef(0);
  const router = useRouter();
  const toast = useToast();

  useEffect(() => {
    setSnapshot((current) => {
      if (
        current.report.overview.work_id !== initial.report.overview.work_id ||
        current.taskGraph.basis.branch_id !== initial.taskGraph.basis.branch_id
      ) {
        return initial;
      }
      return initial.report.overview.event_head > current.report.overview.event_head
        ? initial
        : current;
    });
  }, [initial]);

  const overview = snapshot.report.overview;
  const branchId = selectedBranch.branch_id;
  const branchLabel = workBranchLabel(branchCatalog, selectedBranch);
  const isDeliveryBranch = selectedBranch.is_delivery;
  const deliveryBranch = branchCatalog.branches.find((branch) => branch.is_delivery)!;
  const delivery = deliveryLabels[snapshot.report.finding.fact_code];
  const pending = snapshot.proposals.proposals;

  useEffect(() => {
    setArchivePage(archivedBranches ?? null);
  }, [archivedBranches]);

  const acceptDeletionOperation = useCallback(
    (operation: WorkBranchDeletionOperationV1) => {
      if (operation.state === "pending") {
        setDeletionConfirmation(null);
        setDeletionOperation({ ...operation });
        setArchivePage((current) =>
          current && current.work_revision < operation.work_revision
            ? { ...current, work_revision: operation.work_revision }
            : current,
        );
        return;
      }
      setDeletionOperation(null);
      setDeletionConfirmation(null);
      actionIds.current.delete(`delete:${operation.branch_id}`);
      if (operation.state === "succeeded" && operation.outcome === "deleted") {
        setArchivePage((current) =>
          current
            ? {
                ...current,
                work_revision: Math.max(current.work_revision, operation.work_revision),
                branches: current.branches.filter(
                  (branch) => branch.branch_id !== operation.branch_id,
                ),
              }
            : current,
        );
        toast.addToast("Archived approach permanently deleted.", "info");
        router.refresh();
        return;
      }
      setRetentionError(
        operation.outcome === "delivery_branch_protected"
          ? "The Main result cannot be deleted. Loading the current approaches."
          : "The Work changed before deletion began. Loading the current approaches.",
      );
      router.refresh();
    },
    [router, toast],
  );

  const observeDeletion = useCallback(async () => {
    if (!deletionOperation || deletionOperation.state !== "pending") return;
    try {
      const result = await observeWorkBranchDeletionAction({
        workId: overview.work_id,
        branchId: deletionOperation.branch_id,
        operationId: deletionOperation.operation_id,
      });
      if (!result.ok) {
        setRetentionError(
          result.retryable
            ? "Deletion is still converging. Its durable state is safe to check again."
            : "Deletion can no longer be observed. Loading the current approaches.",
        );
        if (!result.retryable) router.refresh();
        return;
      }
      acceptDeletionOperation(result.operation);
    } catch {
      setRetentionError(
        "Deletion is still converging. Its durable state is safe to check again.",
      );
    }
  }, [acceptDeletionOperation, deletionOperation, overview.work_id, router]);

  useEffect(() => {
    if (deletionOperation?.state !== "pending") return;
    const delay = DELETION_POLL_DELAYS_MS[deletionPollAttempt.current];
    if (delay === undefined) return;
    const timer = window.setTimeout(() => {
      deletionPollAttempt.current += 1;
      void observeDeletion();
    }, delay);
    return () => window.clearTimeout(timer);
  }, [deletionOperation, observeDeletion]);

  const acceptForkOperation = useCallback(
    (operation: WorkBranchCreationOperationV1) => {
      if (operation.state === "pending") {
        // Observation may return referentially identical data; each durable
        // observation still advances the bounded polling schedule.
        setForkOperation({ ...operation });
        return;
      }
      setForkOperation(null);
      forkRequestId.current = null;
      if (operation.state === "succeeded" && operation.outcome === "created") {
        router.push(
          `/works/${encodeURIComponent(overview.work_id)}?branch=${encodeURIComponent(operation.child_branch_id)}`,
        );
        return;
      }
      if (
        operation.outcome === "branch_revision_conflict" ||
        operation.outcome === "cursor_conflict"
      ) {
        setForkError(
          "This approach advanced before the alternative was created. Loading its latest saved turn.",
        );
        router.refresh();
        return;
      }
      setForkError(
        operation.outcome === "capacity_exceeded"
          ? "This Work already has the maximum number of active alternatives."
          : "Alternative creation stopped before it became visible.",
      );
    },
    [overview.work_id, router],
  );

  useEffect(() => {
    if (forkOperation?.state !== "pending") return;
    const pollDelay = FORK_POLL_DELAYS_MS[forkPollAttempt.current];
    if (pollDelay === undefined) {
      setForkOperation(null);
      setForkError(
        "The alternative is taking longer than expected. Try again to check the same durable request.",
      );
      return;
    }
    let cancelled = false;
    const timer = window.setTimeout(async () => {
      try {
        forkPollAttempt.current += 1;
        const result = await observeWorkBranchCreationAction({
          workId: overview.work_id,
          originBranchId: branchId,
          operationId: forkOperation.operation_id,
        });
        if (cancelled) return;
        if (!result.ok) {
          setForkOperation(null);
          setForkError(
            result.retryable
              ? "The alternative is still being created. Try again to check its durable result."
              : "The alternative could not be observed.",
          );
          return;
        }
        acceptForkOperation(result.operation);
      } catch {
        if (!cancelled) {
          setForkOperation(null);
          setForkError(
            "The alternative is still being created. Try again to check its durable result.",
          );
        }
      }
    }, pollDelay);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [acceptForkOperation, branchId, forkOperation, overview.work_id]);

  useEffect(() => {
    forkRequestId.current = null;
    forkPollAttempt.current = 0;
    setForkOperation(null);
    setForkError(null);
  }, [branchId]);

  useEffect(() => {
    comparisonGeneration.current += 1;
    setComparison(null);
    setComparisonError(null);
    setComparisonLoading(false);
    setSelectionError(null);
    setSelectionPending(false);
    selectionRequestId.current = null;
  }, [branchId, deliveryBranch.branch_id]);

  async function compareWithMainResult() {
    if (isDeliveryBranch || comparisonLoading) return;
    const generation = comparisonGeneration.current + 1;
    comparisonGeneration.current = generation;
    setComparisonError(null);
    setComparisonLoading(true);
    try {
      const result = await compareWorkBranchesAction({
        workId: overview.work_id,
        leftBranchId: branchId,
        rightBranchId: deliveryBranch.branch_id,
      });
      if (comparisonGeneration.current !== generation) return;
      if (!result.ok) {
        if (result.status === 404) {
          setComparisonError(
            "One of these approaches is no longer available. Loading the current Work.",
          );
          router.refresh();
        } else {
          setComparisonError(
            result.retryable
              ? "The comparison is temporarily unavailable. You can safely try again."
              : "These approaches cannot be compared in their current state.",
          );
        }
        return;
      }
      selectionRequestId.current = null;
      setSelectionError(null);
      setComparison(result.comparison);
    } catch {
      if (comparisonGeneration.current === generation) {
        setComparisonError(
          "The comparison is temporarily unavailable. You can safely try again.",
        );
      }
    } finally {
      if (comparisonGeneration.current === generation) {
        setComparisonLoading(false);
      }
    }
  }

  async function selectComparedResult() {
    if (!comparison?.directly_comparable || selectionPending) return;
    const generation = comparisonGeneration.current;
    const requestId =
      selectionRequestId.current ?? `work-delivery:${crypto.randomUUID()}`;
    selectionRequestId.current = requestId;
    setSelectionError(null);
    setSelectionPending(true);
    try {
      const result = await selectWorkDeliveryAction({
        workId: overview.work_id,
        requestId,
        comparison,
      });
      if (comparisonGeneration.current !== generation) return;
      if (!result.ok) {
        if (
          result.status === 409 &&
          result.code === "work_delivery_selection_conflict"
        ) {
          selectionRequestId.current = null;
          setComparison(null);
          setSelectionError(
            "This approach changed after it was compared. Review the refreshed facts before using it.",
          );
          router.refresh();
        } else if (result.status === 404) {
          selectionRequestId.current = null;
          setComparison(null);
          setSelectionError(
            "This approach is no longer available. Loading the current Work.",
          );
          router.refresh();
        } else {
          setSelectionError(
            result.retryable
              ? "The result was not changed. You can safely try the same request again."
              : "This result could not be used in the current Work state.",
          );
        }
        return;
      }
      selectionRequestId.current = null;
      setComparison(null);
      toast.addToast("This approach is now the Main result.", "info");
      router.refresh();
    } catch {
      if (comparisonGeneration.current === generation) {
        setSelectionError(
          "The result was not confirmed. You can safely try the same request again.",
        );
      }
    } finally {
      if (comparisonGeneration.current === generation) {
        setSelectionPending(false);
      }
    }
  }

  async function changeBranchRetention(
    kind: "archive" | "restore",
    branch: Pick<WorkArchivedBranchEntryV1, "branch_id" | "branch_revision">,
    expectedWorkRevision: number,
  ) {
    if (retentionPending) return;
    const actionKey = `${kind}:${branch.branch_id}`;
    const requestId =
      actionIds.current.get(actionKey) ??
      `work-branch:${kind}:${crypto.randomUUID()}`;
    actionIds.current.set(actionKey, requestId);
    setRetentionPending(branch.branch_id);
    setRetentionError(null);
    try {
      const result = await changeWorkBranchRetentionAction({
        workId: overview.work_id,
        branchId: branch.branch_id,
        requestId,
        expectedWorkRevision,
        expectedBranchRevision: branch.branch_revision,
        kind,
      });
      if (!result.ok) {
        if (result.status === 409 && result.code === "work_branch_active") {
          setRetentionError(
            "This approach still has a running turn. Let it finish or stop it before archiving.",
          );
        } else if (result.status === 409 || result.status === 404) {
          actionIds.current.delete(actionKey);
          setRetentionError(
            "The Work changed before this action completed. Loading the current approaches.",
          );
          router.refresh();
        } else {
          setRetentionError(
            result.retryable
              ? "The change was not confirmed. You can safely try the same action again."
              : "This approach cannot be changed in the current Work state.",
          );
        }
        return;
      }
      actionIds.current.delete(actionKey);
      toast.addToast(
        kind === "archive" ? "Approach archived." : "Approach restored.",
        "info",
      );
      const targetBranchId =
        kind === "archive" ? deliveryBranch.branch_id : branch.branch_id;
      router.push(
        `/works/${encodeURIComponent(overview.work_id)}?branch=${encodeURIComponent(targetBranchId)}`,
      );
      router.refresh();
    } catch {
      setRetentionError(
        "The change was not confirmed. You can safely try the same action again.",
      );
    } finally {
      setRetentionPending(null);
    }
  }

  async function deleteArchivedBranch(branch: WorkArchivedBranchEntryV1) {
    if (retentionPending || deletionOperation?.state === "pending") return;
    const actionKey = `delete:${branch.branch_id}`;
    const requestId =
      actionIds.current.get(actionKey) ??
      `work-branch:delete:${crypto.randomUUID()}`;
    actionIds.current.set(actionKey, requestId);
    deletionPollAttempt.current = 0;
    setRetentionPending(branch.branch_id);
    setRetentionError(null);
    try {
      const result = await deleteWorkBranchAction({
        workId: overview.work_id,
        branchId: branch.branch_id,
        requestId,
        expectedWorkRevision: archivePage?.work_revision ?? branchCatalog.work_revision,
        expectedBranchRevision: branch.branch_revision,
      });
      if (!result.ok) {
        if (result.status === 409 || result.status === 404) {
          actionIds.current.delete(actionKey);
          setDeletionConfirmation(null);
          setRetentionError(
            "The Work changed before deletion began. Loading the current approaches.",
          );
          router.refresh();
        } else {
          setRetentionError(
            result.retryable
              ? "Deletion was not confirmed. You can safely retry the same durable request."
              : "This archived approach cannot be deleted in its current state.",
          );
        }
        return;
      }
      acceptDeletionOperation(result.operation);
    } catch {
      setRetentionError(
        "Deletion was not confirmed. You can safely retry the same durable request.",
      );
    } finally {
      setRetentionPending(null);
    }
  }

  async function loadMoreArchivedBranches() {
    const cursor = archivePage?.next_cursor;
    if (!archivePage || !cursor || archivePageLoading) return;
    const basis = archivePage;
    setArchivePageLoading(true);
    setRetentionError(null);
    try {
      const result = await loadArchivedWorkBranchesAction({
        workId: overview.work_id,
        before: cursor,
      });
      if (!result.ok) {
        setRetentionError(
          result.retryable
            ? "Archived approaches are temporarily unavailable. You can safely try again."
            : "Archived approaches changed. Loading the current Work.",
        );
        if (!result.retryable) router.refresh();
        return;
      }
      if (result.page.work_revision !== basis.work_revision) {
        setRetentionError(
          "The Work changed while archived approaches were loading. Loading the current Work.",
        );
        router.refresh();
        return;
      }
      setArchivePage({
        ...result.page,
        branches: [...basis.branches, ...result.page.branches],
      });
    } catch {
      setRetentionError(
        "Archived approaches are temporarily unavailable. You can safely try again.",
      );
    } finally {
      setArchivePageLoading(false);
    }
  }

  async function createAlternative() {
    if (!attachment?.head || forkStarting || forkOperation?.state === "pending") return;
    setForkError(null);
    setForkStarting(true);
    forkPollAttempt.current = 0;
    const requestId =
      forkRequestId.current ?? `work-alternative:${crypto.randomUUID()}`;
    forkRequestId.current = requestId;
    try {
      const result = await createWorkBranchAction({
        workId: overview.work_id,
        originBranchId: branchId,
        requestId,
        expectedBranchRevision: attachment.branch_revision,
        committedCursor: attachment.head,
      });
      if (!result.ok) {
        if (!result.retryable) forkRequestId.current = null;
        setForkError(
          result.retryable
            ? "Alternative creation did not finish. It is safe to try again."
            : "An alternative cannot be created from the current saved turn.",
        );
        return;
      }
      acceptForkOperation(result.operation);
    } catch {
      setForkError("Alternative creation did not finish. It is safe to try again.");
    } finally {
      setForkStarting(false);
    }
  }

  async function stopAlternativeCreation() {
    if (forkOperation?.state !== "pending" || forkStopping) return;
    setForkStopping(true);
    try {
      const result = await abortWorkBranchCreationAction({
        workId: overview.work_id,
        originBranchId: branchId,
        operationId: forkOperation.operation_id,
      });
      if (!result.ok) {
        setForkError(
          result.retryable
            ? "The stop request did not finish. The operation remains safe to observe."
            : "The alternative already reached a durable result.",
        );
        return;
      }
      forkRequestId.current = null;
      forkPollAttempt.current = 0;
      setForkOperation(null);
      setForkError("Alternative creation stopped before it became visible.");
    } catch {
      setForkError("The stop request did not finish. The operation remains safe to observe.");
    } finally {
      setForkStopping(false);
    }
  }

  async function toggleProposal(proposal: WorkCriteriaProposalSummaryV1) {
    setReviewError(null);
    if (expandedProposalId === proposal.proposal_id) {
      setExpandedProposalId(null);
      return;
    }
    setExpandedProposalId(proposal.proposal_id);
    if (proposalDetails[proposal.proposal_id]) return;

    await loadProposalDetail(proposal);
  }

  async function loadProposalDetail(proposal: WorkCriteriaProposalSummaryV1) {
    setReviewError(null);
    setLoadingProposalId(proposal.proposal_id);
    try {
      const result = await loadCriteriaProposalAction({
        workId: overview.work_id,
        branchId,
        proposalId: proposal.proposal_id,
      });
      if (!result.ok) {
        handleActionError(result);
        return;
      }
      setProposalDetails((current) => ({
        ...current,
        [proposal.proposal_id]: result.detail,
      }));
    } catch {
      setReviewError("The suggestion could not be loaded. You can safely try again.");
    } finally {
      setLoadingProposalId(null);
    }
  }

  function handleActionError(error: WorkActionError) {
    if (error.status === 401) {
      setReviewError("Your sign-in expired. Sign in again before reviewing this suggestion.");
      return;
    }
    if (error.status === 403) {
      setReviewError("You do not have permission to review this Work.");
      return;
    }
    if (error.status === 404) {
      setReviewError("This suggestion is no longer available. Loading the latest Work state.");
      router.refresh();
      return;
    }
    if (error.status === 409 || error.status === 412) {
      setReviewError("The Work changed before this decision was applied. Loading the latest state.");
      router.refresh();
      return;
    }
    setReviewError(
      error.retryable
        ? "The decision did not complete. You can safely try it again."
        : "The decision could not be applied to the current Work state.",
    );
  }

  async function resolveProposal(
    proposal: WorkCriteriaProposalSummaryV1,
    decision: "accept" | "reject",
  ) {
    setReviewError(null);
    setResolvingProposalId(proposal.proposal_id);
    const actionKey = `${proposal.proposal_id}:${decision}`;
    let requestId = actionIds.current.get(actionKey);
    if (!requestId) {
      requestId = `work-criteria:${decision}:${crypto.randomUUID()}`;
      actionIds.current.set(actionKey, requestId);
    }

    try {
      const result = await resolveCriteriaProposalAction({
        workId: overview.work_id,
        branchId,
        proposal,
        decision,
        requestId,
      });
      if (!result.ok) {
        handleActionError(result);
        return;
      }
      actionIds.current.delete(actionKey);
      setSnapshot(result.snapshot);
      setExpandedProposalId(null);
      setProposalDetails((current) => {
        const next = { ...current };
        delete next[proposal.proposal_id];
        return next;
      });
      toast.addToast(
        decision === "accept"
          ? "Done-when criteria accepted."
          : "Done-when suggestion rejected.",
        "info",
      );
    } catch {
      setReviewError("The decision did not complete. You can safely try it again.");
    } finally {
      setResolvingProposalId(null);
    }
  }

  return (
    <div className="h-full overflow-y-auto">
      <main className="mx-auto w-full max-w-6xl px-5 py-8 sm:px-8 lg:py-12">
        <header className="flex flex-col gap-5 border-b border-border/80 pb-8 sm:flex-row sm:items-start sm:justify-between">
          <div className="min-w-0">
            <p className="text-xs font-semibold uppercase tracking-[0.12em] text-text-muted">
              Work · {branchLabel}
            </p>
            <h1 className="mt-2 max-w-3xl text-2xl font-semibold leading-tight tracking-[-0.025em] text-text sm:text-3xl">
              {overview.goal.goal}
            </h1>
            <p className="mt-3 text-xs tabular-nums text-text-muted">
              {attachment
                ? attachment.head
                  ? `Synced · ${attachment.head.completed_turn} committed ${attachment.head.completed_turn === 1 ? "turn" : "turns"}`
                  : "Synced · no committed turns yet"
                : "Live continuity unavailable · durable Work facts remain readable"}
            </p>
          </div>
          <div className="flex shrink-0 flex-col items-end gap-2">
            <div className="flex items-center gap-2 rounded-full bg-surface-muted px-3 py-1.5 text-sm font-medium text-text-secondary">
              <span className={cn("size-2 rounded-full", delivery.tone)} />
              {isDeliveryBranch ? delivery.label : `Main result · ${delivery.label}`}
            </div>
            {branchCatalog.branches.length > 1 ? (
              <label className="flex items-center gap-2 text-xs text-text-muted">
                Approach
                <select
                  aria-label="Work approach"
                  className="rounded-control border border-border bg-surface px-2 py-1 text-sm font-medium text-text outline-none focus:border-accent"
                  value={branchId}
                  onChange={(event) =>
                    router.push(
                      `/works/${encodeURIComponent(overview.work_id)}?branch=${encodeURIComponent(event.target.value)}`,
                    )
                  }
                >
                  {branchCatalog.branches.map((branch) => (
                    <option key={branch.branch_id} value={branch.branch_id}>
                      {workBranchLabel(branchCatalog, branch)}
                    </option>
                  ))}
                </select>
              </label>
            ) : null}
          </div>
        </header>

        <div className="mt-8 grid gap-8 lg:grid-cols-[minmax(0,1fr)_320px]">
          <div className="min-w-0 space-y-6">
            <Card className="space-y-4">
              <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
                <div>
                  <p className="text-sm font-semibold text-text">{branchLabel}</p>
                  <p className="mt-1 text-xs leading-5 text-text-muted">
                    {attachment?.head
                      ? "Starts a separate approach after the latest saved turn. Work still running stays here; tools and workspace access are checked again."
                      : "A separate approach becomes available after the first turn is saved."}
                  </p>
                  {selectedBranch.materialization ? (
                    <ForkMaterializationSummary
                      materialization={selectedBranch.materialization}
                    />
                  ) : null}
                  {forkError ? (
                    <p role="alert" className="mt-2 text-xs leading-5 text-danger">
                      {forkError}
                    </p>
                  ) : null}
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  {!isDeliveryBranch ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      leadingIcon={Archive}
                      disabled={retentionPending !== null}
                      onClick={() =>
                        void changeBranchRetention(
                          "archive",
                          selectedBranch,
                          branchCatalog.work_revision,
                        )
                      }
                    >
                      {retentionPending === branchId ? "Archiving…" : "Archive"}
                    </Button>
                  ) : null}
                  {!isDeliveryBranch ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      leadingIcon={GitBranch}
                      disabled={comparisonLoading}
                      onClick={() => void compareWithMainResult()}
                    >
                      {comparisonLoading ? "Comparing…" : "Compare with Main result"}
                    </Button>
                  ) : null}
                  {forkOperation?.state === "pending" ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={forkStopping}
                      onClick={() => void stopAlternativeCreation()}
                    >
                      {forkStopping ? "Stopping…" : "Stop creating"}
                    </Button>
                  ) : (
                    <Button
                      size="sm"
                      variant="secondary"
                      leadingIcon={GitFork}
                      disabled={!attachment?.head || forkStarting}
                      onClick={() => void createAlternative()}
                    >
                      {forkStarting ? "Creating…" : "Try another approach"}
                    </Button>
                  )}
                </div>
              </div>
              {comparisonError ? (
                <p role="alert" className="text-xs leading-5 text-danger">
                  {comparisonError}
                </p>
              ) : null}
              {selectionError ? (
                <p role="alert" className="text-xs leading-5 text-danger">
                  {selectionError}
                </p>
              ) : null}
              {retentionError ? (
                <p role="alert" className="text-xs leading-5 text-danger">
                  {retentionError}
                </p>
              ) : null}
              {comparison ? (
                <BranchComparisonSummary
                  comparison={comparison}
                  selectionPending={selectionPending}
                  onSelect={() => void selectComparedResult()}
                />
              ) : null}
              {archivePage && archivePage.branches.length > 0 ? (
                <section
                  aria-label="Archived approaches"
                  className="border-t border-border/70 pt-4"
                >
                  <p className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
                    Archived approaches
                  </p>
                  <ul className="mt-2 space-y-1">
                    {archivePage.branches.map((branch) => {
                      const deleting =
                        deletionOperation?.branch_id === branch.branch_id &&
                        deletionOperation.state === "pending";
                      const confirming = deletionConfirmation === branch.branch_id;
                      return (
                        <li
                          key={branch.branch_id}
                          className="rounded-control px-2 py-1.5 hover:bg-surface-muted/60"
                        >
                          <div className="flex items-center justify-between gap-3">
                            <span className="min-w-0 truncate text-xs text-text-secondary">
                              {deleting
                                ? deletionProgressLabel(deletionOperation.phase)
                                : `Archived ${ARCHIVE_DATE_FORMATTER.format(new Date(branch.archived_at))}`}
                            </span>
                            {confirming ? (
                              <div className="flex shrink-0 items-center gap-1">
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  disabled={retentionPending !== null}
                                  onClick={() => setDeletionConfirmation(null)}
                                >
                                  Cancel
                                </Button>
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  leadingIcon={Trash2}
                                  disabled={retentionPending !== null}
                                  onClick={() => void deleteArchivedBranch(branch)}
                                >
                                  {retentionPending === branch.branch_id
                                    ? "Deleting…"
                                    : "Delete permanently"}
                                </Button>
                              </div>
                            ) : deleting ? (
                              <Button
                                size="sm"
                                variant="ghost"
                                onClick={() => void observeDeletion()}
                              >
                                Check progress
                              </Button>
                            ) : (
                              <div className="flex shrink-0 items-center gap-1">
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  leadingIcon={ArchiveRestore}
                                  disabled={
                                    retentionPending !== null ||
                                    deletionOperation?.state === "pending"
                                  }
                                  onClick={() =>
                                    void changeBranchRetention(
                                      "restore",
                                      branch,
                                      archivePage.work_revision,
                                    )
                                  }
                                >
                                  {retentionPending === branch.branch_id
                                    ? "Restoring…"
                                    : "Restore"}
                                </Button>
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  leadingIcon={Trash2}
                                  disabled={
                                    retentionPending !== null ||
                                    deletionOperation?.state === "pending"
                                  }
                                  onClick={() =>
                                    setDeletionConfirmation(branch.branch_id)
                                  }
                                >
                                  Delete
                                </Button>
                              </div>
                            )}
                          </div>
                          {confirming ? (
                            <p className="mt-1 text-xs leading-5 text-danger">
                              Permanently remove this approach, its session, and retained history.
                            </p>
                          ) : null}
                        </li>
                      );
                    })}
                  </ul>
                  {archivePage.next_cursor ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={archivePageLoading || retentionPending !== null}
                      onClick={() => void loadMoreArchivedBranches()}
                    >
                      {archivePageLoading ? "Loading…" : "Show more archived"}
                    </Button>
                  ) : null}
                </section>
              ) : null}
            </Card>

            <WorkTranscriptCard
              workId={overview.work_id}
              branchId={branchId}
              initial={transcript}
            />

            <WorkTurnComposer
              workId={overview.work_id}
              branchId={branchId}
              attachmentId={attachment?.attachment_id}
              branchRevision={attachment?.branch_revision}
              controlBasis={attachment?.control_basis}
              initialDraft={overview.goal.goal}
              onActivityChange={setTurnActive}
            />

            {patchArtifacts !== undefined ? (
              <WorkPatchReviewCard
                key={branchId}
                workId={overview.work_id}
                branchId={branchId}
                initial={patchArtifacts}
                initialMaterializations={patchMaterializations ?? null}
                initialCommits={patchCommits ?? null}
                exportBasis={{
                  branchRevision: selectedBranch.branch_revision,
                  graphRevision: selectedBranch.current_graph_revision,
                }}
                materializeTarget={
                  isDeliveryBranch
                    ? undefined
                    : {
                        branchId: deliveryBranch.branch_id,
                        label: "Main result",
                        branchRevision: deliveryBranch.branch_revision,
                        graphRevision: deliveryBranch.current_graph_revision,
                      }
                }
                commitTarget={{
                  branchId: deliveryBranch.branch_id,
                  label: "Main result",
                  branchRevision: deliveryBranch.branch_revision,
                  graphRevision: deliveryBranch.current_graph_revision,
                }}
              />
            ) : null}

            <WorkActivityCard
              workId={overview.work_id}
              activity={snapshot.activity}
            />

            {pending.length > 0 ? (
              <Card className="overflow-hidden p-0">
                <div className="flex items-start gap-3 px-5 py-4">
                  <div className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
                    <ClipboardCheck className="size-4" />
                  </div>
                  <div className="min-w-0 flex-1">
                    <h2 className="text-sm font-semibold text-text">
                      Suggested Done when
                    </h2>
                    <p className="mt-1 text-sm leading-6 text-text-secondary">
                      Astra proposed completion criteria. Work can continue while you
                      review them when convenient.
                    </p>
                  </div>
                  <span className="rounded-full bg-surface-muted px-2 py-0.5 text-xs tabular-nums text-text-muted">
                    {pending.length}
                  </span>
                </div>

                {reviewError ? (
                  <div role="alert" className="border-t border-danger/20 bg-danger/5 px-5 py-3 text-sm text-danger">
                    {reviewError}
                  </div>
                ) : null}

                <div className="divide-y divide-border/70 border-t border-border/70">
                  {pending.map((proposal) => {
                    const expanded = expandedProposalId === proposal.proposal_id;
                    const detail = proposalDetails[proposal.proposal_id];
                    const loading = loadingProposalId === proposal.proposal_id;
                    const resolving = resolvingProposalId === proposal.proposal_id;
                    return (
                      <section key={proposal.proposal_id}>
                        <button
                          type="button"
                          className="flex w-full items-center gap-3 px-5 py-3 text-left hover:bg-surface-muted/60"
                          onClick={() => void toggleProposal(proposal)}
                          aria-expanded={expanded}
                        >
                          {expanded ? (
                            <ChevronDown className="size-4 shrink-0 text-text-muted" />
                          ) : (
                            <ChevronRight className="size-4 shrink-0 text-text-muted" />
                          )}
                          <span className="min-w-0 flex-1 text-sm font-medium text-text">
                            Review {proposal.member_count} completion {proposal.member_count === 1 ? "criterion" : "criteria"}
                          </span>
                          <span className="text-xs text-text-muted">
                            {proposal.source_kind === "reflection" ? "Review" : "Astra"}
                          </span>
                        </button>
                        {expanded ? (
                          <div className="border-t border-border/60 bg-bg/50 px-5 py-4">
                            {loading ? (
                              <p className="text-sm text-text-muted">Loading criteria…</p>
                            ) : detail ? (
                              <ProposalDetail
                                detail={detail}
                                accepted={snapshot.criteria.criteria.entries}
                                busy={resolving}
                                onAccept={() => void resolveProposal(proposal, "accept")}
                                onReject={() => void resolveProposal(proposal, "reject")}
                              />
                            ) : (
                              <button
                                type="button"
                                className="text-sm font-medium text-accent hover:underline"
                                onClick={() => void loadProposalDetail(proposal)}
                              >
                                Try loading again
                              </button>
                            )}
                          </div>
                        ) : null}
                      </section>
                    );
                  })}
                </div>
              </Card>
            ) : null}

            <WorkTaskGraph initial={snapshot.taskGraph} live={turnActive} />

            <Card>
              <div className="flex items-center justify-between gap-3">
                <h2 className="text-sm font-semibold text-text">Accepted Done when</h2>
                <span className="text-xs tabular-nums text-text-muted">
                  {snapshot.criteria.criteria.total}
                </span>
              </div>
              {snapshot.criteria.criteria.entries.length > 0 ? (
                <ul className="mt-4 space-y-3">
                  {snapshot.criteria.criteria.entries.map((criterion) => (
                    <CriterionRow key={criterion.criterion_id} criterion={criterion} />
                  ))}
                </ul>
              ) : (
                <p className="mt-3 text-sm leading-6 text-text-secondary">
                  No criteria are accepted yet. Astra cannot mark this Work done
                  until completion criteria are explicitly accepted.
                </p>
              )}
              {snapshot.criteria.criteria.total > snapshot.criteria.criteria.entries.length ? (
                <p className="mt-4 text-xs text-text-muted">
                  Showing {snapshot.criteria.criteria.entries.length} of {snapshot.criteria.criteria.total} criteria.
                </p>
              ) : null}
            </Card>
          </div>

          <aside className="space-y-5">
            <Card>
              <p className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
                Main result status
              </p>
              <p className="mt-3 text-sm font-semibold text-text">{delivery.label}</p>
              <p className="mt-1 text-sm leading-6 text-text-secondary">
                {delivery.detail}
              </p>
              <dl className="mt-5 space-y-3 border-t border-border/70 pt-4 text-sm">
                <SummaryRow
                  icon={<ListChecks className="size-4" />}
                  label="Plan items"
                  value={overview.graph.item_count}
                />
                <SummaryRow
                  icon={<Check className="size-4" />}
                  label="Criteria satisfied"
                  value={`${overview.delivery.satisfied_criterion_count}/${overview.delivery.required_criterion_count}`}
                />
                <SummaryRow
                  icon={<GitBranch className="size-4" />}
                  label="Branch"
                  value={branchLabel}
                />
              </dl>
            </Card>
          </aside>
        </div>
      </main>
    </div>
  );
}

function BranchComparisonSummary({
  comparison,
  selectionPending,
  onSelect,
}: {
  comparison: WorkBranchComparisonReportV2;
  selectionPending: boolean;
  onSelect: () => void;
}) {
  const relationLabel = {
    same: "Same",
    different: "Different",
    unavailable: "Not available",
  } as const;
  const gapLabel = {
    change_details: "change details",
    risks: "risks",
    time_cost: "time and cost",
  } as const;
  return (
    <section
      aria-label="Approach comparison"
      className="border-t border-border/70 pt-4"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <p className="text-sm font-semibold text-text">
            {comparison.directly_comparable
              ? "Comparable foundation"
              : "Not directly comparable"}
          </p>
          <p className="mt-1 text-xs leading-5 text-text-muted">
            Exact saved facts only. Astra has not chosen a preferred approach.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "rounded-full px-2.5 py-1 text-xs font-medium",
              comparison.directly_comparable
                ? "bg-success/10 text-success"
                : "bg-warning/10 text-warning",
            )}
          >
            {comparison.directly_comparable ? "Same basis" : "Basis changed"}
          </span>
          {comparison.directly_comparable ? (
            <Button
              size="sm"
              leadingIcon={Check}
              disabled={selectionPending}
              onClick={onSelect}
            >
              {selectionPending ? "Using…" : "Use this result"}
            </Button>
          ) : null}
        </div>
      </div>
      <dl className="mt-4 grid gap-2 text-xs sm:grid-cols-2">
        <ComparisonFact
          label="Goal"
          value={
            comparison.blockers.includes("goal_revision_differs")
              ? "Different revision"
              : "Same revision"
          }
        />
        <ComparisonFact
          label="Done when"
          value={
            comparison.blockers.includes("criteria_revision_differs")
              ? "Different revision"
              : `Same revision · ${comparison.left.criteria.member_count} criteria`
          }
        />
        <ComparisonFact
          label="Plan"
          value={`${relationLabel[comparison.graph_relation]} · ${comparison.left.graph.item_count} vs ${comparison.right.graph.item_count} items`}
        />
        <ComparisonFact
          label="Current result"
          value={relationLabel[comparison.subject_relation]}
        />
        <ComparisonFact
          label="Fresh checks"
          value={`${comparison.left_evidence.fresh_check_count}/${comparison.left_evidence.required_count} vs ${comparison.right_evidence.fresh_check_count}/${comparison.right_evidence.required_count}`}
        />
        <ComparisonFact
          label="Accepted gaps"
          value={`${comparison.left_evidence.accepted_gap_count} vs ${comparison.right_evidence.accepted_gap_count}`}
        />
      </dl>
      {comparison.coverage_gaps.length > 0 ? (
        <p className="mt-4 text-xs leading-5 text-text-muted">
          Not compared yet: {comparison.coverage_gaps.map((gap) => gapLabel[gap]).join(", ")}.
        </p>
      ) : null}
    </section>
  );
}

function ComparisonFact({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3 rounded-control bg-surface-muted/60 px-3 py-2">
      <dt className="text-text-muted">{label}</dt>
      <dd className="text-right font-medium text-text-secondary">{value}</dd>
    </div>
  );
}

function workBranchLabel(
  catalog: WorkBranchCatalogV1,
  branch: WorkBranchCatalogEntryV1,
): string {
  if (branch.is_delivery) return "Main result";
  const alternativeIndex = catalog.branches
    .filter((candidate) => !candidate.is_delivery)
    .findIndex((candidate) => candidate.branch_id === branch.branch_id);
  return alternativeIndex >= 0 ? `Alternative ${alternativeIndex + 1}` : "Alternative";
}

function ForkMaterializationSummary({
  materialization,
}: {
  materialization: NonNullable<WorkBranchCatalogEntryV1["materialization"]>;
}) {
  const dimensionLabels: Record<
    (typeof materialization)[number]["dimension"],
    string
  > = {
    conversation: "conversation",
    goal: "goal",
    criteria: "Done when",
    task_graph: "plan",
    checkpoint: "checkpoint",
    workspace: "workspace",
    artifacts: "artifacts",
    transient_authority: "active runs and approvals",
  };
  const dispositionLabels: Record<
    (typeof materialization)[number]["disposition"],
    string
  > = {
    shared: "Kept",
    copied: "Copied",
    rebased: "Rebased",
    gap: "Needs setup",
    excluded: "Not carried",
  };
  const groups = new Map<string, string[]>();
  for (const item of materialization) {
    const label = dispositionLabels[item.disposition];
    groups.set(label, [...(groups.get(label) ?? []), dimensionLabels[item.dimension]]);
  }
  return (
    <dl className="mt-2 space-y-0.5 text-[11px] leading-5 text-text-muted">
      {[...groups].map(([label, dimensions]) => (
        <div key={label} className="flex gap-1.5">
          <dt className="font-medium text-text-secondary">{label}</dt>
          <dd>{dimensions.join(", ")}</dd>
        </div>
      ))}
    </dl>
  );
}

function ProposalDetail({
  detail,
  accepted,
  busy,
  onAccept,
  onReject,
}: {
  detail: WorkCriteriaProposalDetailV1;
  accepted: WorkCriterionV1[];
  busy: boolean;
  onAccept: () => void;
  onReject: () => void;
}) {
  const acceptedById = new Map(accepted.map((criterion) => [criterion.criterion_id, criterion]));
  return (
    <div>
      <ul className="space-y-3">
        {detail.members.map((member) => (
          <ProposalMember
            key={member.criterion_id}
            member={member}
            accepted={acceptedById.get(member.criterion_id)}
          />
        ))}
      </ul>
      <div className="mt-5 flex flex-wrap items-center gap-2 border-t border-border/70 pt-4">
        <Button size="sm" variant="primary" leadingIcon={Check} disabled={busy} onClick={onAccept}>
          {busy ? "Applying…" : "Accept criteria"}
        </Button>
        <Button size="sm" variant="ghost" leadingIcon={X} disabled={busy} onClick={onReject}>
          Reject suggestion
        </Button>
      </div>
    </div>
  );
}

function ProposalMember({
  member,
  accepted,
}: {
  member: WorkCriteriaProposalMemberV1;
  accepted?: WorkCriterionV1;
}) {
  if (member.member_kind === "existing") {
    return accepted ? (
      <CriterionRow criterion={accepted} prefix="Keep" />
    ) : (
      <li className="flex items-start gap-3 text-sm">
        <CircleDot className="mt-1 size-3.5 shrink-0 text-text-muted" />
        <div>
          <p className="text-text-secondary">Keep an accepted criterion</p>
          <p className="mt-0.5 font-mono text-xs text-text-muted">
            {member.criterion_id} · revision {member.revision}
          </p>
        </div>
      </li>
    );
  }

  return (
    <li className="flex items-start gap-3 text-sm">
      <CircleDot className="mt-1 size-3.5 shrink-0 text-accent" />
      <div className="min-w-0">
        <p className="leading-6 text-text">{member.definition.statement}</p>
        {member.definition.kind !== "human_review" ? (
          <code className="mt-1 block overflow-x-auto rounded-control bg-surface-muted px-2 py-1 font-mono text-xs text-text-secondary">
            {member.definition.command}
          </code>
        ) : (
          <p className="mt-0.5 text-xs text-text-muted">Human review</p>
        )}
      </div>
    </li>
  );
}

function CriterionRow({
  criterion,
  prefix,
}: {
  criterion: WorkCriterionV1;
  prefix?: string;
}) {
  return (
    <li className="flex items-start gap-3 text-sm">
      <CircleDot className="mt-1 size-3.5 shrink-0 text-success" />
      <div className="min-w-0">
        <p className="leading-6 text-text">
          {prefix ? <span className="mr-1 text-text-muted">{prefix}:</span> : null}
          {criterion.statement}
        </p>
        {criterion.kind !== "human_review" ? (
          <code className="mt-1 block overflow-x-auto rounded-control bg-surface-muted px-2 py-1 font-mono text-xs text-text-secondary">
            {criterion.command}
          </code>
        ) : null}
      </div>
    </li>
  );
}

function SummaryRow({
  icon,
  label,
  value,
}: {
  icon: React.ReactNode;
  label: string;
  value: string | number;
}) {
  return (
    <div className="flex items-center gap-2 text-text-secondary">
      <span className="text-text-muted">{icon}</span>
      <dt className="min-w-0 flex-1">{label}</dt>
      <dd className="font-medium tabular-nums text-text">{value}</dd>
    </div>
  );
}
