export type PendingRunApproval = {
  requestId: string;
  tool: string;
  detail?: string;
  displayLabel?: string;
  sessionId: string;
  runId: string;
  approvalKind: "standard" | "explicit" | "protected_bash_write";
};

export function pendingRunApproval(
  value: unknown,
  fallbackRunId?: string,
): PendingRunApproval | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const event = value as Record<string, unknown>;
  const runId = typeof event.run_id === "string" && event.run_id.trim()
    ? event.run_id : fallbackRunId;
  if (event.type !== "approval_required" || !runId ||
    typeof event.request_id !== "string" || !event.request_id.trim() ||
    typeof event.tool !== "string" || !event.tool.trim() ||
    typeof event.session_id !== "string" || !event.session_id.trim()) return null;
  return {
    requestId: event.request_id,
    tool: event.tool,
    detail: typeof event.detail === "string" ? event.detail : undefined,
    displayLabel: typeof event.display_label === "string" ? event.display_label : undefined,
    sessionId: event.session_id,
    runId,
    approvalKind: event.approval_kind === "protected_bash_write"
      ? "protected_bash_write" : event.approval_kind === "explicit" ? "explicit" : "standard",
  };
}
