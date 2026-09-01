import type { WorkTaskGraphItemV2 } from "@astra/sdk";

export type WorkTaskTone = "neutral" | "running" | "warning" | "danger" | "success";

export type WorkTaskPresentation = {
  label: string;
  tone: WorkTaskTone;
  verified: boolean;
  needsAttention: boolean;
};

/**
 * Project the three independent Task facts into one user-facing state.
 *
 * Declared state owns whether the item still exists, execution owns whether an
 * attempt is active/terminal, delivery owns what that attempt produced, and
 * verification owns whether current evidence supports the result. A terminal
 * Run alone is deliberately never presented as completed work.
 */
export function workTaskPresentation(item: WorkTaskGraphItemV2): WorkTaskPresentation {
  if (item.declaration_state !== "active") {
    return {
      label: item.declaration_state === "cancelled" ? "Cancelled" : "Replaced",
      tone: "neutral",
      verified: false,
      needsAttention: false,
    };
  }

  if (item.delivery.status === "blocked") {
    return { label: "Blocked", tone: "warning", verified: false, needsAttention: true };
  }
  if (item.delivery.status === "failed") {
    return { label: "Failed", tone: "danger", verified: false, needsAttention: true };
  }

  switch (item.execution.status) {
    case "running":
    case "delegated":
      return { label: "Working", tone: "running", verified: false, needsAttention: false };
    case "waiting":
      return { label: "Waiting", tone: "warning", verified: false, needsAttention: true };
    case "paused":
      return { label: "Paused", tone: "warning", verified: false, needsAttention: true };
    case "failed":
      return { label: "Failed", tone: "danger", verified: false, needsAttention: true };
    case "cancelled":
      return { label: "Cancelled", tone: "warning", verified: false, needsAttention: true };
    case "not_started":
      return { label: "Planned", tone: "neutral", verified: false, needsAttention: false };
    case "completed":
      break;
  }

  if (item.delivery.status === "unreported") {
    return {
      label: "Result not reported",
      tone: "warning",
      verified: false,
      needsAttention: true,
    };
  }

  const check = item.verification.latest_check;
  if (
    item.verification.status === "evidence_available" &&
    check?.freshness === "current" &&
    check.outcome === "passed" &&
    check.coverage === "complete" &&
    check.evidence_ref_count > 0
  ) {
    return { label: "Verified", tone: "success", verified: true, needsAttention: false };
  }
  if (
    item.verification.status === "evidence_available" &&
    check?.freshness === "current" &&
    check.outcome !== "passed"
  ) {
    return { label: "Check failed", tone: "danger", verified: false, needsAttention: true };
  }
  return {
    label: item.verification.status === "stale_evidence" ? "Needs recheck" : "Needs verification",
    tone: "warning",
    verified: false,
    needsAttention: true,
  };
}

export function workTaskNeedsAttention(item: WorkTaskGraphItemV2): boolean {
  return workTaskPresentation(item).needsAttention;
}

export function isWorkTaskOpen(item: WorkTaskGraphItemV2): boolean {
  const state = workTaskPresentation(item);
  return item.declaration_state === "active" && !state.verified;
}

export function workTaskCounts(items: WorkTaskGraphItemV2[]) {
  let working = 0;
  let planned = 0;
  let attention = 0;
  let verified = 0;
  let open = 0;
  for (const item of items) {
    const state = workTaskPresentation(item);
    if (item.declaration_state !== "active") continue;
    if (state.verified) verified += 1;
    else open += 1;
    if (["running", "delegated"].includes(item.execution.status)) working += 1;
    if (item.execution.status === "not_started") planned += 1;
    if (state.needsAttention) attention += 1;
  }
  return { working, planned, attention, verified, open };
}
