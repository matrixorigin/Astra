import { EXPLAIN_ANALYZE_HTML_STYLE } from "./explain-analyze-html-style";
import { layoutExplainAnalyzeGraph } from "./explain-analyze-layout";
import { renderExplainAnalyzeText } from "./explain-analyze-text";
import type {
  ExplainAnalyzeEventV1,
  ExplainAnalyzeCoverageGapV1,
  ExplainAnalyzeUsageV1,
  ExplainAnalyzeContextMetricsV1,
  ExplainAnalyzeAuxiliaryDetailsV1,
  ExplainAnalyzeAdmissionSettlementV1,
  ExplainAnalyzeRequestJudgmentResultV1,
  ExplainAnalyzeRequestJudgmentFieldV1,
  MemorySelectionReport,
} from "./types";

export type ExplainAnalyzeNodeV1 = {
  nodeId: string;
  runId: string;
  turnId: string;
  clockDomainId: string;
  kind: ExplainAnalyzeEventV1["kind"];
  label: string;
  parentNodeId?: string;
  dependencyNodeIds: string[];
  roundIndex?: number;
  attemptIndex?: number;
  startElapsedMs: number;
  endElapsedMs?: number;
  durationMs?: number;
  outcome?: ExplainAnalyzeEventV1["outcome"];
  usage?: ExplainAnalyzeUsageV1;
  context?: ExplainAnalyzeContextMetricsV1;
  auxiliaryUsage?: ExplainAnalyzeEventV1["auxiliary_usage"];
  auxiliaryDetails?: ExplainAnalyzeEventV1["auxiliary_details"];
  coverageGaps: ExplainAnalyzeCoverageGapV1[];
  startObserved: boolean;
  terminalObserved: boolean;
  conflicted: boolean;
};

export type ExplainAnalyzeDiagnosticV1 = {
  code:
    | "invalid_event"
    | "conflicting_fact"
    | "missing_parent"
    | "missing_dependency"
    | "parent_cycle"
    | "dependency_cycle"
    | "unresolved_terminal_node";
  nodeId?: string;
  relatedNodeId?: string;
};

export type ExplainAnalyzeGraphV1 = {
  /** Internal consistency of observed facts, not a claim of instrumentation coverage. */
  integrity: "consistent" | "unknown";
  diagnostics: ExplainAnalyzeDiagnosticV1[];
  nodes: ExplainAnalyzeNodeV1[];
  duplicateEventCount: number;
  conflictedNodeIds: string[];
  coverageGaps: ExplainAnalyzeCoverageGapV1[];
  /** Sticky evidence loss when a conflicting incoming turn fact is discarded. */
  auxiliaryCaptureConflicted?: boolean;
};

const nodeKinds = new Set([
  "run",
  "turn",
  "admission",
  "preparation",
  "context_assembly",
  "judgment",
  "model_round",
  "provider_attempt",
  "tool_batch",
  "tool_call",
  "wait",
  "child_run",
  "settlement",
]);
const outcomes = new Set([
  "completed",
  "succeeded",
  "failed",
  "cancelled",
  "interrupted",
  "blocked",
  "waiting",
  "rejected",
  "reused",
  "suppressed",
  "deferred",
  "resolved",
  "fallback",
  "unavailable",
  "delegated",
]);
const allowedEventKeys = new Set([
  "type",
  "index",
  "schema_version",
  "event_id",
  "run_id",
  "turn_id",
  "node_id",
  "parent_node_id",
  "dependency_node_ids",
  "producer_id",
  "clock_domain_id",
  "kind",
  "round_index",
  "attempt_index",
  "label",
  "transition",
  "elapsed_ms",
  "start_elapsed_ms",
  "duration_ms",
  "outcome",
  "usage",
  "context",
  "coverage_gaps", "auxiliary_usage", "auxiliary_details",
]);
const coverageGaps = new Set<ExplainAnalyzeCoverageGapV1>([
  "user_input_wait_intervals",
  "provider_retry_backoff",
  "first_token_latency",
  "child_run_intervals",
  "tool_io_wait_intervals",
  "approval_wait_intervals",
]);

/** Runtime validation for events received over SSE or restored from storage. */
export function isExplainAnalyzeEventV1(
  value: unknown,
): value is ExplainAnalyzeEventV1 {
  if (!isRecord(value) || Object.keys(value).some((key) => !allowedEventKeys.has(key))) {
    return false;
  }
  if (
    value.type !== "explain_analyze" ||
    value.schema_version !== 1 ||
    !nonEmptyString(value.event_id, 512) ||
    !nonEmptyString(value.run_id, 512) ||
    !nonEmptyString(value.turn_id, 512) ||
    !nonEmptyString(value.node_id, 512) ||
    !nonEmptyString(value.producer_id, 512) ||
    !nonEmptyString(value.clock_domain_id, 512) ||
    !nonEmptyString(value.label, 160) ||
    (typeof value.kind !== "string" || !nodeKinds.has(value.kind)) ||
    !isNonNegativeInteger(value.elapsed_ms) ||
    (value.round_index !== undefined && !isNonNegativeInteger(value.round_index)) ||
    (value.attempt_index !== undefined && !isNonNegativeInteger(value.attempt_index)) ||
    (value.parent_node_id !== undefined && !nonEmptyString(value.parent_node_id, 512)) ||
    (value.dependency_node_ids !== undefined &&
      (!Array.isArray(value.dependency_node_ids) ||
        !value.dependency_node_ids.every((id) => nonEmptyString(id, 512))))
  ) {
    return false;
  }
  if (value.auxiliary_usage !== undefined &&
      (value.kind !== "turn" || value.transition !== "finished" || !isAuxiliaryUsage(value.auxiliary_usage))) return false;
  if (value.auxiliary_details !== undefined &&
      (value.kind !== "turn" || value.transition !== "finished" || !isExplainAnalyzeAuxiliaryDetails(value.auxiliary_details, value.elapsed_ms))) return false;
  if (value.coverage_gaps !== undefined &&
    (value.kind !== "turn" || value.transition !== "finished" ||
      !isCoverageGapList(value.coverage_gaps))) {
    return false;
  }
  if (
    (value.kind === "model_round" && value.round_index === undefined) ||
    (value.kind === "provider_attempt" &&
      (value.round_index === undefined || value.attempt_index === undefined))
  ) {
    return false;
  }
  if (value.transition === "started") {
    return (
      value.start_elapsed_ms === undefined &&
      value.duration_ms === undefined &&
      value.outcome === undefined &&
      value.usage === undefined && value.context === undefined && value.coverage_gaps === undefined
        && value.auxiliary_details === undefined
    );
  }
  if (
    value.transition !== "finished" ||
    !isNonNegativeInteger(value.start_elapsed_ms) ||
    !isNonNegativeInteger(value.duration_ms) ||
    (typeof value.outcome !== "string" || !outcomes.has(value.outcome)) ||
    value.start_elapsed_ms > value.elapsed_ms ||
    Math.abs(value.elapsed_ms - value.start_elapsed_ms - value.duration_ms) > 1
  ) {
    return false;
  }
  return (value.usage === undefined || isExplainAnalyzeUsage(value.usage)) &&
    (value.context === undefined ||
      (value.usage === undefined && (value.kind === "context_assembly" || value.kind === "preparation") && isExplainContext(value.context, value.kind)));
}

function isAuxiliaryUsage(value: unknown): boolean {
  if (!isRecord(value) || Object.keys(value).some(k => !["available", "truncated", "attempts"].includes(k)) ||
      typeof value.available !== "boolean" ||
      (value.truncated !== undefined && typeof value.truncated !== "boolean") ||
      !Array.isArray(value.attempts) || (!value.available && (value.attempts.length > 0 || value.truncated === true))) return false;
  const seen = new Set<string>();
  return value.attempts.every(a => {
    if (!isRecord(a) || Object.keys(a).some(k => !["attempt_id","usage_status","provider","offering_id","model_name","purpose","operation_id","usage"].includes(k))) return false;
    for (const key of ["attempt_id","provider","offering_id","purpose","operation_id"]) {
      if (!nonEmptyString(a[key],512) || /[\s\u0000-\u001f\u007f]/u.test(a[key] as string)) return false;
    }
    if (!nonEmptyString(a.model_name,255) || /[\u0000-\u001f\u007f]/u.test(a.model_name) || seen.has(a.attempt_id as string)) return false;
    seen.add(a.attempt_id as string);
    if (!["provider_exact","provider_partial","unavailable"].includes(a.usage_status as string)) return false;
    if (a.usage_status === "unavailable") return a.usage === undefined;
    if (a.usage === undefined) return a.usage_status === "provider_partial";
    return isExplainAnalyzeUsage(a.usage) && a.usage.basis === a.usage_status;
  });
}

const admissionStatuses = new Set([
  "accepted", "rejected", "unavailable", "not_dispatched",
]);
const admissionReasonKinds = new Set([
  "accepted", "classifier_uncertain", "classifier_conflicting",
  "invalid_classifier_response", "provider_rejected", "planning_rejected",
  "reconciliation_rejected", "unavailable", "not_dispatched",
]);
const judgmentFields = new Set<ExplainAnalyzeRequestJudgmentFieldV1>([
  "required", "defer", "mutation.read_only", "mutation.may_mutate",
  "mutation.must_mutate", "scope.workspace", "scope.external", "scope.mixed",
  "scope.unknown", "domain.none", "domain.github", "domain.git", "domain.code",
  "domain.memory", "domain.web", "domain.system", "domain.database",
  "parallel_subruns", "capability.web",
]);
const judgmentDomains = new Set([
  "github", "git", "code", "memory", "web", "system", "database",
]);
const judgmentMutations = new Set(["read_only", "may_mutate", "must_mutate"]);
const judgmentScopes = new Set(["workspace", "external", "mixed", "unknown"]);
const judgmentCapabilities = new Set(["web", "agent_spawner"]);
const invalidJudgmentReasons = new Set([
  "malformed_json", "invalid_contract", "unsupported_combination",
]);
const preDispatchReasons = new Set([
  "no_offering", "capacity_pressure", "invalid_request", "output_budget",
  "route_unavailable", "durable_material_unavailable", "preparation_deadline",
  "cancelled",
]);
const unavailableReasons = new Set([
  "execution_error", "deadline", "cancelled", "provider_ptl_error",
  "unexpected_finish",
]);
const judgmentDeliveries = new Set(["unresolved", "response_received"]);

function isExplainAnalyzeAuxiliaryDetails(
  value: unknown,
  terminalElapsedMs: number,
): value is ExplainAnalyzeAuxiliaryDetailsV1 {
  const calls = isRecord(value) && value.calls === undefined ? [] : isRecord(value) ? value.calls : undefined;
  if (!isRecord(value) || Object.keys(value).some((key) => !["calls", "truncated", "admission"].includes(key)) ||
      (value.truncated !== undefined && typeof value.truncated !== "boolean") ||
      !Array.isArray(calls) || calls.length > 16 ||
      !calls.every((call) => isExplainAnalyzeAuxiliaryCall(call, terminalElapsedMs))) return false;
  const callIds = new Set<string>();
  if (calls.some((call) => {
    const id = isRecord(call) ? call.call_id : undefined;
    if (typeof id !== "string" || callIds.has(id)) return true;
    callIds.add(id);
    return false;
  })) return false;
  return value.admission === undefined || isExplainAnalyzeAdmissionSettlement(value.admission);
}

function isExplainAnalyzeAuxiliaryCall(value: unknown, terminalElapsedMs: number): boolean {
  if (!isRecord(value) || Object.keys(value).some((key) =>
    !["call_id", "operation_id", "stage", "start_elapsed_ms", "duration_ms", "outcome"].includes(key))) return false;
  return isExplainId(value.call_id) && isExplainId(value.operation_id) && isExplainId(value.stage) &&
    isNonNegativeInteger(value.start_elapsed_ms) && isNonNegativeInteger(value.duration_ms) &&
    value.start_elapsed_ms <= terminalElapsedMs &&
    value.duration_ms <= terminalElapsedMs - value.start_elapsed_ms + 1 &&
    typeof value.outcome === "string" && outcomes.has(value.outcome);
}

function isExplainId(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0 &&
    new TextEncoder().encode(value).length <= 512;
}

function isExplainAnalyzeAdmissionSettlement(value: unknown): value is ExplainAnalyzeAdmissionSettlementV1 {
  if (!isRecord(value) || Object.keys(value).some((key) =>
    !["status", "reason", "classification", "decision"].includes(key)) ||
      typeof value.status !== "string" || !admissionStatuses.has(value.status) ||
      !isExplainAnalyzeAdmissionReason(value.reason)) return false;
  if (value.classification !== undefined && !isExplainAnalyzeRequestJudgmentResult(value.classification)) return false;
  if (value.decision !== undefined && !isExplainAnalyzeRequestJudgmentResult(value.decision)) return false;
  const reason = value.reason as Record<string, unknown>;
  switch (value.status) {
    case "accepted":
      return reason.kind === "accepted" &&
        value.decision !== undefined && value.decision.result === "decided";
    case "rejected":
      return reason.kind !== "accepted" && reason.kind !== "unavailable" &&
        reason.kind !== "not_dispatched" && value.decision === undefined;
    case "unavailable":
      return reason.kind === "unavailable" && value.decision === undefined;
    case "not_dispatched":
      return reason.kind === "not_dispatched" && value.decision === undefined;
    default:
      return false;
  }
}

function isExplainAnalyzeAdmissionReason(value: unknown): boolean {
  if (!isRecord(value) || Object.keys(value).some((key) => !["kind", "reason"].includes(key)) ||
      typeof value.kind !== "string" || !admissionReasonKinds.has(value.kind)) return false;
  if (value.kind === "unavailable") {
    return typeof value.reason === "string" && unavailableReasons.has(value.reason);
  }
  if (value.kind === "not_dispatched") {
    return typeof value.reason === "string" && preDispatchReasons.has(value.reason);
  }
  return value.reason === undefined;
}

function isExplainAnalyzeRequestJudgmentResult(
  value: unknown,
): value is ExplainAnalyzeRequestJudgmentResultV1 {
  if (!isRecord(value) || typeof value.result !== "string") return false;
  switch (value.result) {
    case "decided": {
      if (Object.keys(value).some((key) => !["result", "classification"].includes(key)) ||
          !isRecord(value.classification) ||
          Object.keys(value.classification).some((key) => ![
            "work_required", "activation_deferred", "domain", "mutation", "scope",
            "parallel_subruns", "capabilities",
          ].includes(key))) return false;
      const classification = value.classification;
      return typeof classification.work_required === "boolean" &&
        typeof classification.activation_deferred === "boolean" &&
        (classification.domain === null || (typeof classification.domain === "string" && judgmentDomains.has(classification.domain))) &&
        typeof classification.mutation === "string" && judgmentMutations.has(classification.mutation) &&
        typeof classification.scope === "string" && judgmentScopes.has(classification.scope) &&
        typeof classification.parallel_subruns === "boolean" &&
        Array.isArray(classification.capabilities) && classification.capabilities.length <= 2 &&
        classification.capabilities.every((capability) => typeof capability === "string" && judgmentCapabilities.has(capability)) &&
        new Set(classification.capabilities).size === classification.capabilities.length &&
        (!classification.activation_deferred || classification.work_required) &&
        (classification.mutation === "must_mutate" || classification.scope === "unknown");
    }
    case "abstained": {
      if (Object.keys(value).some((key) => !["result", "uncertain_fields", "assessment"].includes(key)) ||
          !isJudgmentFieldList(value.uncertain_fields) || !isRecord(value.assessment) ||
          Object.keys(value.assessment).some((key) => !["provenance", "fields"].includes(key)) ||
          typeof value.assessment.provenance !== "string" ||
          !["provider_probability", "discrete_decision"].includes(value.assessment.provenance) ||
          !Array.isArray(value.assessment.fields) || value.assessment.fields.length === 0 ||
          value.assessment.fields.length > 19) return false;
      const fields = value.assessment.fields;
      const seen = new Set<string>();
      if (fields.some((field) => !isRecord(field) || Object.keys(field).some((key) => !["field", "score"].includes(key)) ||
          typeof field.field !== "string" || !judgmentFields.has(field.field as ExplainAnalyzeRequestJudgmentFieldV1) ||
          seen.has(field.field) || (seen.add(field.field), typeof field.score !== "number" || !Number.isFinite(field.score) || field.score < 0 || field.score > 1))) return false;
      if (value.assessment.provenance === "discrete_decision" && fields.some((field) => ![0, 0.5, 1].includes(field.score))) return false;
      return value.uncertain_fields.every((field) => fields.some((assessment) => assessment.field === field));
    }
    case "conflicting":
      return Object.keys(value).every((key) => ["result", "fields"].includes(key)) && isJudgmentFieldList(value.fields);
    case "invalid":
      return Object.keys(value).every((key) => ["result", "reason"].includes(key)) &&
        typeof value.reason === "string" && invalidJudgmentReasons.has(value.reason);
    case "not_dispatched":
      return Object.keys(value).every((key) => ["result", "reason"].includes(key)) &&
        typeof value.reason === "string" && preDispatchReasons.has(value.reason);
    case "unavailable":
      return Object.keys(value).every((key) => ["result", "reason", "delivery"].includes(key)) &&
        typeof value.reason === "string" && unavailableReasons.has(value.reason) &&
        typeof value.delivery === "string" && judgmentDeliveries.has(value.delivery) &&
        ((value.reason === "provider_ptl_error" || value.reason === "unexpected_finish") ===
          (value.delivery === "response_received"));
    default:
      return false;
  }
}

function isJudgmentFieldList(value: unknown): value is ExplainAnalyzeRequestJudgmentFieldV1[] {
  return Array.isArray(value) && value.length > 0 && value.length <= 19 &&
    value.every((field) => typeof field === "string" && judgmentFields.has(field as ExplainAnalyzeRequestJudgmentFieldV1)) &&
    new Set(value).size === value.length;
}

/** Auxiliary physical attempts are separate from timed main-model node usage. */
export function explainAnalyzeAuxiliaryUsageLines(graph: ExplainAnalyzeGraphV1): string[] {
  type Attempt = NonNullable<ExplainAnalyzeEventV1["auxiliary_usage"]>["attempts"][number];
  const attempts = new Map<string, Attempt>();
  const known = new Map<string, (number | undefined)[]>();
  const buckets = (a: Attempt) => [a.usage?.fresh_input_tokens, a.usage?.output_tokens, a.usage?.cache_read_tokens, a.usage?.cache_creation_tokens];
  const identity = (a: Attempt) => JSON.stringify([a.provider, a.offering_id, a.model_name, a.purpose, a.operation_id]);
  const rank = { unavailable: 0, provider_partial: 1, provider_exact: 2 };
  const preferred = (a: Attempt, b: Attempt) => {
    const left = [rank[a.usage_status], Number(!!a.usage), ...buckets(a).map(v => v ?? -1)];
    const right = [rank[b.usage_status], Number(!!b.usage), ...buckets(b).map(v => v ?? -1)];
    for (let i = 0; i < left.length; i++) if (left[i] !== right[i]) return left[i] > right[i];
    return false;
  };
  let conflicted = graph.auxiliaryCaptureConflicted === true || graph.nodes.some(n => n.conflicted && (n.kind === "turn" || !!n.auxiliaryUsage || !!n.auxiliaryDetails));
  let unavailable = false;
  let truncated = false;
  for (const node of graph.nodes) {
    if (!node.terminalObserved || node.conflicted || !node.auxiliaryUsage) continue;
    unavailable ||= !node.auxiliaryUsage.available;
    truncated ||= node.auxiliaryUsage.truncated === true;
    for (const attempt of node.auxiliaryUsage.attempts) {
      const existing = attempts.get(attempt.attempt_id);
      const previous = known.get(attempt.attempt_id) ?? buckets(attempt);
      if (existing) conflicted ||= identity(existing) !== identity(attempt);
      buckets(attempt).forEach((value, index) => {
        if (value !== undefined) {
          if (previous[index] !== undefined && previous[index] !== value) conflicted = true;
          previous[index] ??= value;
        }
      });
      known.set(attempt.attempt_id, previous);
      // Consensus is only conflict evidence; never synthesize a richer record.
      if (!existing || preferred(attempt, existing)) attempts.set(attempt.attempt_id, attempt);
    }
  }
  if (conflicted) return ["Auxiliary tokens · capture unavailable: conflicting attribution or usage facts; totals unavailable"];
  const groups = new Map<string, Attempt[]>();
  for (const attempt of attempts.values()) {
    const key = JSON.stringify([attempt.provider,attempt.offering_id,attempt.model_name,attempt.purpose,attempt.operation_id]);
    const group = groups.get(key) ?? []; group.push(attempt); groups.set(key,group);
  }
  const purposeLabels = new Map<string,string>([["memory_retrieval_rerank","Memory judgment"], ["tool_result_rerank","Tool result selection"], ["memory_extraction","Memory extraction"], ["introspection","Request analysis"], ["verification_judge","Verification"], ["reflection","Reflection"], ["required_compaction","Context summary"]]);
  const operationLabels = new Map<string,string>([["request_judgment","Request classification"], ["skill_auto_route","Skill selection"], ["work_plan","Work planning"]]);
  const lines = [...groups.entries()].sort(([a],[b]) => a.localeCompare(b)).map(([,group]) => {
    const first = group[0]; const reported = group.flatMap(a => a.usage ? [a.usage] : []);
    const provider = first.provider === "typesafe" ? "Jev" : first.provider;
    const lanes = [["in","fresh_input_tokens"],["cache read","cache_read_tokens"],["cache write","cache_creation_tokens"],["out","output_tokens"]] as const;
    const values = reported.length === 0 ? "usage unavailable" : lanes.map(([name,key]) => {
      const counters = reported.flatMap(u => u[key] === undefined ? [] : [BigInt(u[key])]);
      const qualifier = truncated || unavailable || counters.length !== group.length ? "at least " : "";
      return `${name} ${counters.length === 0 ? "unknown" : qualifier + counters.reduce((a,b)=>a+b,0n).toString()}`;
    }).join(" · ");
    const partial = reported.length !== group.length || group.some(a => a.usage_status === "provider_partial") ? " · partial" : "";
    const scope = truncated || unavailable ? " captured" : "";
    return `Auxiliary tokens · ${provider} (${first.model_name}) · ${operationLabels.get(first.operation_id) ?? purposeLabels.get(first.purpose) ?? "Auxiliary inference"} · operation ${first.operation_id} · offering ${first.offering_id} · ${values} · ${reported.length}/${group.length}${scope} requests reported${partial}`;
  });
  if (unavailable) lines.push("Auxiliary tokens · capture unavailable");
  if (truncated) lines.push("Auxiliary tokens · capture truncated; request counts cover captured attempts only; token sums are lower bounds");
  return lines;
}

/** Render logical auxiliary timing and typed admission facts separately from
 * physical provider usage. Intervals may overlap and are never added together. */
export function explainAnalyzeAuxiliaryDetailsLines(graph: ExplainAnalyzeGraphV1): string[] {
  const terminalTurns = graph.nodes
    .filter((node) => node.kind === "turn" && node.terminalObserved && !node.conflicted && node.auxiliaryDetails)
    .sort((left, right) => left.nodeId.localeCompare(right.nodeId));
  const lines: string[] = [];
  for (const node of terminalTurns) {
    const details = node.auxiliaryDetails!;
    lines.push(`Auxiliary scope · ${node.nodeId}`);
    for (const call of [...(details.calls ?? [])].sort((left, right) =>
      left.start_elapsed_ms - right.start_elapsed_ms || left.call_id.localeCompare(right.call_id))) {
      lines.push(`Auxiliary timing · ${call.operation_id} · call ${call.call_id} · operation ${call.operation_id} · stage ${call.stage} · ${formatMs(call.duration_ms)} · client outcome ${call.outcome} · starts +${formatMs(call.start_elapsed_ms)} · logical client interval; overlapping intervals are not added`);
    }
    if (details.truncated === true) {
      lines.push("Auxiliary timing · capture truncated · only the bounded set of logical calls is shown");
    }
    if (details.admission) {
      const admission = details.admission;
      const status = admission.status.replace("_", " ");
      const jsonOrUnavailable = (value: unknown) => value === undefined ? "unavailable" : JSON.stringify(value);
      lines.push(`Admission settlement · status ${status} · reason ${jsonOrUnavailable(admission.reason)} · classifier result ${jsonOrUnavailable(admission.classification)} · reconciled decision ${jsonOrUnavailable(admission.decision)}`);
    }
  }
  return lines;
}

function isCoverageGapList(value: unknown): value is ExplainAnalyzeCoverageGapV1[] {
  if (!Array.isArray(value) || value.length > coverageGaps.size ||
    !value.every((gap): gap is ExplainAnalyzeCoverageGapV1 =>
      typeof gap === "string" && coverageGaps.has(gap as ExplainAnalyzeCoverageGapV1))) {
    return false;
  }
  return new Set(value).size === value.length &&
    value.every((gap, index) => index === 0 || value[index - 1] < gap);
}

const contextSourceKinds = new Set([
  "identity", "self_model", "project_context", "deferred_tools", "available_skills",
  "memory", "working_memory", "history", "constraints", "skills", "runtime_identity",
  "runtime_volatile", "emergent_skills", "emergent_memory", "emergent_summary",
]);
const budgetFields = ["estimated_input_tokens", "estimated_system_tokens", "tool_schema_tokens",
  "requested_output_tokens", "reserved_protocol_tokens", "effective_input_limit_tokens",
  "model_context_limit_tokens", "visible_tool_count"] as const;
const budgetKeys = new Set<string>(["basis", ...budgetFields]);
function isExplainContext(value: unknown, kind: string): value is ExplainAnalyzeContextMetricsV1 {
  if (!isRecord(value) || Object.keys(value).some((key) => key !== "budget" && key !== "assembly") ||
    (value.budget === undefined && value.assembly === undefined)) return false;
  if (value.budget !== undefined) {
    if (kind !== "preparation") return false;
    const budget = value.budget;
    if (!isRecord(budget) || budget.basis !== "pre_provider_estimate" ||
      Object.keys(budget).some((key) => !budgetKeys.has(key)) ||
      budgetFields.some((key) => !isNonNegativeInteger(budget[key])) ||
      (budget.visible_tool_count as number) > 0xffff_ffff) return false;
  }
  if (value.assembly !== undefined) {
    if (kind !== "context_assembly") return false;
    const assembly = value.assembly;
    if (!isRecord(assembly) || assembly.basis !== "runtime_text_estimate" ||
      Object.keys(assembly).some((key) => !["basis", "sources", "edge_memory_selection"].includes(key)) ||
      !Array.isArray(assembly.sources) || assembly.sources.length > contextSourceKinds.size) return false;
    if (assembly.edge_memory_selection !== undefined && (!Array.isArray(assembly.edge_memory_selection) || assembly.edge_memory_selection.length > 2 || !assembly.edge_memory_selection.every(isMemorySelectionReport))) return false;
    const seen = new Set<string>();
    for (const source of assembly.sources) {
      if (!isRecord(source) || typeof source.kind !== "string" || !contextSourceKinds.has(source.kind) ||
        seen.has(source.kind) ||
        Object.keys(source).some((key) => !["kind", "section_count", "estimated_tokens"].includes(key)) ||
        !isNonNegativeInteger(source.section_count) || source.section_count > 0xffff_ffff ||
        !isNonNegativeInteger(source.estimated_tokens)) return false;
      seen.add(source.kind);
    }
  }
  return true;
}

function isMemorySelectionReport(value: unknown): value is MemorySelectionReport {
  if (!isRecord(value) || Object.keys(value).some(k => !["session_id", "turn", "operation", "method", "reason", "model", "candidates", "selection_order", "elapsed_ms", "candidate_coverage", "prompt_projection"].includes(k)) ||
      typeof value.session_id !== "string" || value.session_id.length === 0 || new TextEncoder().encode(value.session_id).length > 512 || /[\u0000-\u001f\u007f-\u009f]/u.test(value.session_id) ||
      !isNonNegativeInteger(value.turn) || value.turn === 0 || value.turn > 0xffff_ffff ||
      !["relevance", "dismissal", "reuse"].includes(String(value.operation)) ||
      !["model", "lexical", "none", "reuse"].includes(String(value.method)) ||
      !["completed", "no_candidates", "no_selector", "call_unavailable", "invalid_response", "retrieval_unavailable", "retrieval_timeout", "reused"].includes(String(value.reason)) ||
      !(value.model === null || (typeof value.model === "string" && value.model.trim().length > 0 && new TextEncoder().encode(value.model).length <= 160 && !/[\u0000-\u001f\u007f-\u009f]/u.test(value.model))) ||
      !isNonNegativeInteger(value.elapsed_ms) || !Array.isArray(value.candidates) || value.candidates.length > 256) return false;
  if (!value.candidates.every((c, i) => isRecord(c) && c.index === i && typeof c.selected === "boolean" &&
    Object.keys(c).every(k => ["index", "selected", "probability_bps"].includes(k)) &&
    (c.probability_bps === null || (isNonNegativeInteger(c.probability_bps) && c.probability_bps <= 10000)))) return false;
  const r = value as MemorySelectionReport;
  if (!Array.isArray(r.selection_order) || r.selection_order.length !== r.candidates.filter(c => c.selected).length ||
      new Set(r.selection_order).size !== r.selection_order.length || !r.selection_order.every(i => isNonNegativeInteger(i) && r.candidates[i]?.selected)) return false;
  if (r.candidate_coverage !== undefined &&
      (!isRecord(r.candidate_coverage) || Object.keys(r.candidate_coverage).some(k => !["source_items", "evaluated_candidates", "truncated"].includes(k)) ||
       !isNonNegativeInteger(r.candidate_coverage.source_items) || !isNonNegativeInteger(r.candidate_coverage.evaluated_candidates) ||
       r.candidate_coverage.evaluated_candidates !== r.candidates.length || r.candidate_coverage.source_items < r.candidate_coverage.evaluated_candidates ||
       typeof r.candidate_coverage.truncated !== "boolean")) return false;
  if (r.prompt_projection !== undefined &&
      (!isRecord(r.prompt_projection) || Object.keys(r.prompt_projection).some(k => !["selected_candidates", "included_candidates"].includes(k)) ||
       r.operation === "dismissal" || !isNonNegativeInteger(r.prompt_projection.selected_candidates) ||
       r.prompt_projection.selected_candidates !== r.selection_order.length ||
       !(r.prompt_projection.included_candidates === null ||
         (isNonNegativeInteger(r.prompt_projection.included_candidates) && r.prompt_projection.included_candidates <= r.prompt_projection.selected_candidates)))) return false;
  const noScores = r.candidates.every(c => c.probability_bps === null);
  switch (r.reason) {
    case "completed": return r.method === "model" && r.operation !== "reuse" && r.model !== null && r.candidates.length > 0;
    case "no_candidates": return r.method === "none" && r.operation !== "reuse" && r.model === null && r.candidates.length === 0;
    case "retrieval_unavailable": case "retrieval_timeout": return r.method === "none" && r.operation === "relevance" && r.model === null && r.candidates.length === 0;
    case "reused": return r.method === "reuse" && r.operation === "reuse" && r.model === null && noScores && r.candidates.every(c => c.selected);
    default: return noScores && r.candidates.length > 0 && (r.operation === "relevance" ? r.method === "lexical" : r.operation === "dismissal" && r.method === "none" && r.candidates.every(c => !c.selected));
  }
}

export function memorySelectionLines(report: MemorySelectionReport): string[] {
  const reasons = { completed: "completed", no_candidates: "no candidates", no_selector: report.operation === "dismissal" ? "no selector available; memories kept" : "no selector available; ranked locally, candidates kept",
    call_unavailable: report.operation === "dismissal" ? "selector unavailable; memories kept" : "selector unavailable; ranked locally, candidates kept", invalid_response: report.operation === "dismissal" ? "invalid selector response; memories kept" : "invalid selector response; ranked locally, candidates kept",
    retrieval_unavailable: "retrieval failed", retrieval_timeout: "retrieval timed out", reused: "no new relevance check" };
  const selected = report.candidates.filter(c => c.selected).length;
  const action = report.operation === "dismissal" ? "dismissed" : report.operation === "reuse" ? "reused" : "selected";
  const method = report.method === "model" ? report.model ?? "model" : report.method === "lexical" ? "local keyword matching" : report.method === "reuse" ? "session cache" : "not run";
  const coverage = report.candidate_coverage?.truncated ? ` · bounded ${report.candidate_coverage.source_items} source items to ${report.candidate_coverage.evaluated_candidates} candidates` : "";
  const projection = report.prompt_projection === undefined ? "" : report.prompt_projection.selected_candidates === 0 ? " · no memory entered request" :
    report.prompt_projection.included_candidates === null ? " · request inclusion unknown" : ` · ${report.prompt_projection.included_candidates}/${report.prompt_projection.selected_candidates} entered request`;
  const summary = report.reason.startsWith("retrieval_") ? `Memory retrieval unavailable · ${reasons[report.reason]}` :
    `Memory selection · ${method} · ${report.candidates.length} candidates → ${selected} ${action} · ${report.elapsed_ms}ms · ${reasons[report.reason]}${coverage}${projection}`;
  const projectionDetail = report.prompt_projection === undefined ? "final request projection unavailable" : "final request projection measured";
  return [summary, `Reported by CLI/Edge · turn ${report.turn} · same decision across request rounds · ${projectionDetail}`, ...report.candidates.map(c => {
    const decision = report.operation === "dismissal" ? (c.selected ? "dismissed" : "kept") : (c.selected ? "selected" : "not selected");
    return `Candidate ${c.index + 1} · ${decision}${c.probability_bps === null ? "" : ` · model score ${(c.probability_bps / 100).toFixed(2)}%`}`;
  })];
}

function isExplainAnalyzeUsage(value: unknown): value is ExplainAnalyzeUsageV1 {
  if (!isRecord(value)) return false;
  const validBasis = typeof value.basis === "string" && ["provider_exact", "provider_partial", "runtime_estimated"].includes(
    value.basis,
  );
  const knownLanes = [
    "fresh_input_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "output_tokens",
  ];
  return (
    validBasis &&
    Object.keys(value).every((key) => key === "basis" || knownLanes.includes(key)) &&
    knownLanes.some((key) => isNonNegativeInteger(value[key])) &&
    knownLanes.every((key) => value[key] === undefined || isNonNegativeInteger(value[key]))
  );
}

const contextSourceLabels: Record<string, string> = {
  identity: "Agent instructions", self_model: "Capabilities", project_context: "Project guidance",
  deferred_tools: "Deferred tools", available_skills: "Skill catalog", memory: "Retrieved memory",
  working_memory: "Working memory", history: "Conversation history", constraints: "Response constraints",
  skills: "Active skills", runtime_identity: "Runtime context", runtime_volatile: "Turn instructions",
  emergent_skills: "Discovered skills", emergent_memory: "Prefetched memory", emergent_summary: "Tool summaries",
};

/** Shared readable projection; assembly estimates never enter billed token totals. */
export function explainAnalyzeContextSections(context: ExplainAnalyzeContextMetricsV1) {
  const sections: Array<{ title: string; description: string; rows: Array<{ label: string; value: string }> }> = [];
  if (context.budget) {
    const budget = context.budget;
    sections.push({ title: "Request budget", description: "Estimated for this request before the provider call. Not billed usage.",
      rows: ([
        ["Input estimate", budget.estimated_input_tokens], ["Input limit", budget.effective_input_limit_tokens],
        ["System messages", budget.estimated_system_tokens], ["Tool schemas", budget.tool_schema_tokens],
        ["Output allowance", budget.requested_output_tokens], ["Protocol reserve", budget.reserved_protocol_tokens],
        ["Model context limit", budget.model_context_limit_tokens],
      ] as const).map(([label, count]): { label: string; value: string } => ({ label, value: `${count.toLocaleString("en-US")} tokens` }))
        .concat([{ label: "Visible tools", value: String(budget.visible_tool_count) }]) });
  }
  if (context.assembly) {
    for (const report of context.assembly.edge_memory_selection ?? []) {
      const [summary, ...details] = memorySelectionLines(report);
      sections.push({ title: report.operation === "dismissal" ? "Memory feedback" : report.operation === "reuse" ? "Memory reuse" : "Memory relevance", description: summary,
        rows: details.map((value, index) => index === 0 ? { label: "Observation", value } :
          { label: `Candidate ${index}`, value: value.split(" · ").slice(1).join(" · ") }) });
    }
    sections.push({ title: "Context sources", description: "Text estimates at assembly time. Later request preparation may change the input.",
      rows: context.assembly.sources.map((source) => ({ label: contextSourceLabels[source.kind],
        value: `${source.estimated_tokens.toLocaleString("en-US")} tokens · ${source.section_count} ${source.section_count === 1 ? "section" : "sections"}` })) });
  }
  return sections;
}

export function formatExplainAnalyzeContext(context: ExplainAnalyzeContextMetricsV1): string {
  if (context.budget) return `Input ≈${context.budget.estimated_input_tokens.toLocaleString("en-US")} / ${context.budget.effective_input_limit_tokens.toLocaleString("en-US")}`;
  if (context.assembly?.edge_memory_selection?.length) return context.assembly.edge_memory_selection.map(r => memorySelectionLines(r)[0]).join(" · ");
  return `${context.assembly?.sources.length ?? 0} context sources`;
}

/** Idempotently rebuild the graph; terminal facts can reconstruct a missed start. */
export function reduceExplainAnalyzeEvents(
  events: readonly unknown[],
): ExplainAnalyzeGraphV1 {
  const seenEvents = new Map<string, string>();
  const nodes = new Map<string, ExplainAnalyzeNodeV1>();
  const conflictedNodeIds = new Set<string>();
  let duplicateEventCount = 0;
  let auxiliaryCaptureConflicted = false;
  const diagnostics: ExplainAnalyzeDiagnosticV1[] = [];

  for (const value of events) {
    if (!isExplainAnalyzeEventV1(value)) {
      if (isRecord(value) && value.type === "explain_analyze") {
        diagnostics.push({ code: "invalid_event" });
      }
      continue;
    }
    const fingerprint = validatedFactFingerprint(value);
    const seen = seenEvents.get(value.event_id);
    if (seen !== undefined) {
      duplicateEventCount += 1;
      if (seen !== fingerprint) {
        auxiliaryCaptureConflicted ||= value.kind === "turn" || !!value.auxiliary_usage || !!value.auxiliary_details;
        conflictedNodeIds.add(value.node_id);
        // A reused event identity can also point at a different node.
        const original = JSON.parse(seen) as { node_id: string };
        conflictedNodeIds.add(original.node_id);
      }
      continue;
    }
    seenEvents.set(value.event_id, fingerprint);

    let node = nodes.get(value.node_id);
    if (!node) {
      node = {
        nodeId: value.node_id,
        runId: value.run_id,
        turnId: value.turn_id,
        clockDomainId: value.clock_domain_id,
        kind: value.kind,
        label: value.label,
        ...(value.parent_node_id ? { parentNodeId: value.parent_node_id } : {}),
        dependencyNodeIds: value.dependency_node_ids ?? [],
        ...(value.round_index !== undefined ? { roundIndex: value.round_index } : {}),
        ...(value.attempt_index !== undefined ? { attemptIndex: value.attempt_index } : {}),
        startElapsedMs:
          value.transition === "started"
            ? value.elapsed_ms
            : (value.start_elapsed_ms ?? value.elapsed_ms),
        coverageGaps: value.coverage_gaps ?? [],
        ...(value.transition === "finished"
          ? {
              endElapsedMs: value.elapsed_ms,
              durationMs: value.duration_ms,
              outcome: value.outcome,
              ...(value.usage ? { usage: value.usage } : {}),
              ...(value.context ? { context: value.context } : {}),
              ...(value.auxiliary_usage ? { auxiliaryUsage: value.auxiliary_usage } : {}),
              ...(value.auxiliary_details ? { auxiliaryDetails: value.auxiliary_details } : {}),
            }
          : {}),
        startObserved: value.transition === "started",
        terminalObserved: value.transition === "finished",
        conflicted: false,
      };
      nodes.set(value.node_id, node);
      continue;
    }

    if (
      node.runId !== value.run_id ||
      node.turnId !== value.turn_id ||
      node.clockDomainId !== value.clock_domain_id ||
      node.kind !== value.kind ||
      node.label !== value.label ||
      node.parentNodeId !== value.parent_node_id ||
      node.roundIndex !== value.round_index ||
      node.attemptIndex !== value.attempt_index ||
      !sameStrings(node.dependencyNodeIds, value.dependency_node_ids ?? [])
    ) {
      node.conflicted = true;
    }

    if (value.transition === "started") {
      if (
        (node.startObserved || node.terminalObserved) &&
        node.startElapsedMs !== value.elapsed_ms
      ) {
        node.conflicted = true;
      } else if (!node.startObserved) {
        node.startElapsedMs = value.elapsed_ms;
        node.startObserved = true;
      }
    } else if (node.terminalObserved) {
      if (
        node.startElapsedMs !== value.start_elapsed_ms ||
        node.endElapsedMs !== value.elapsed_ms ||
        node.durationMs !== value.duration_ms ||
        node.outcome !== value.outcome ||
        stableJson(node.usage ?? null) !== stableJson(value.usage ?? null) ||
        stableJson(node.context ?? null) !== stableJson(value.context ?? null) ||
        stableJson(node.auxiliaryUsage ?? null) !== stableJson(value.auxiliary_usage ?? null) ||
        stableJson(node.auxiliaryDetails ?? null) !== stableJson(value.auxiliary_details ?? null) ||
        stableJson(node.coverageGaps) !== stableJson(value.coverage_gaps ?? [])
      ) {
        node.conflicted = true;
      }
    } else {
      if (node.startObserved && node.startElapsedMs !== value.start_elapsed_ms) {
        node.conflicted = true;
      } else if (!node.startObserved && value.start_elapsed_ms !== undefined) {
        node.startElapsedMs = value.start_elapsed_ms;
      }
      node.endElapsedMs = value.elapsed_ms;
      node.durationMs = value.duration_ms;
      node.outcome = value.outcome;
      node.usage = value.usage;
      node.context = value.context;
      node.auxiliaryUsage = value.auxiliary_usage;
      node.auxiliaryDetails = value.auxiliary_details;
      node.coverageGaps = value.coverage_gaps ?? [];
      node.terminalObserved = true;
    }
    if (node.conflicted) {
      conflictedNodeIds.add(node.nodeId);
      auxiliaryCaptureConflicted ||= value.kind === "turn" || !!value.auxiliary_usage || !!value.auxiliary_details;
    }
  }

  const orderedNodes = [...nodes.values()];
  const nodeById = new Map(orderedNodes.map((node) => [node.nodeId, node]));
  const terminalTurns = new Set(orderedNodes
    .filter((node) => node.kind === "turn" && node.terminalObserved)
    .map((node) => JSON.stringify([node.runId, node.turnId, node.clockDomainId])));
  for (const node of orderedNodes) {
    if (conflictedNodeIds.has(node.nodeId)) node.conflicted = true;
    if (node.parentNodeId && !nodeById.has(node.parentNodeId)) {
      diagnostics.push({ code: "missing_parent", nodeId: node.nodeId, relatedNodeId: node.parentNodeId });
    }
    for (const dependency of node.dependencyNodeIds) {
      if (!nodeById.has(dependency)) {
        diagnostics.push({ code: "missing_dependency", nodeId: node.nodeId, relatedNodeId: dependency });
      }
    }
    if (!node.terminalObserved && terminalTurns.has(JSON.stringify([node.runId, node.turnId, node.clockDomainId]))) {
      diagnostics.push({ code: "unresolved_terminal_node", nodeId: node.nodeId });
    }
  }
  for (const nodeId of conflictedNodeIds) diagnostics.push({ code: "conflicting_fact", nodeId });
  diagnoseCycles(nodeById, "parent_cycle", (node) => node.parentNodeId ? [node.parentNodeId] : [], diagnostics);
  diagnoseCycles(nodeById, "dependency_cycle", (node) => node.dependencyNodeIds, diagnostics);
  diagnostics.sort((left, right) =>
    left.code.localeCompare(right.code) ||
    (left.nodeId ?? "").localeCompare(right.nodeId ?? "") ||
    (left.relatedNodeId ?? "").localeCompare(right.relatedNodeId ?? ""));
  orderedNodes.sort((left, right) => {
    const timelineOrder =
      left.clockDomainId.localeCompare(right.clockDomainId) ||
      left.startElapsedMs - right.startElapsedMs;
    if (timelineOrder !== 0) return timelineOrder;
    return nodeDepth(left, nodeById) - nodeDepth(right, nodeById) ||
      left.nodeId.localeCompare(right.nodeId);
  });
  return {
    integrity: diagnostics.length === 0 ? "consistent" : "unknown",
    diagnostics,
    nodes: orderedNodes,
    duplicateEventCount,
    conflictedNodeIds: [...conflictedNodeIds].sort(),
    coverageGaps: [...new Set(orderedNodes.flatMap((node) => node.coverageGaps))].sort(),
    auxiliaryCaptureConflicted,
  };
}

export function explainAnalyzeCoverageGapLabel(gap: ExplainAnalyzeCoverageGapV1): string {
  switch (gap) {
    case "user_input_wait_intervals": return "user input waits";
    case "provider_retry_backoff": return "provider retry backoff";
    case "first_token_latency": return "time to first token";
    case "child_run_intervals": return "child-run timing";
    case "approval_wait_intervals": return "some approval waits";
    case "tool_io_wait_intervals": return "tool I/O wait breakdown";
  }
}

/** Iterative DFS avoids overflowing the JS stack on long histories. */
function diagnoseCycles(
  nodes: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  code: "parent_cycle" | "dependency_cycle",
  edges: (node: ExplainAnalyzeNodeV1) => readonly string[],
  diagnostics: ExplainAnalyzeDiagnosticV1[],
) {
  const colors = new Map<string, "visiting" | "done">();
  for (const root of [...nodes.keys()].sort()) {
    if (colors.has(root)) continue;
    const stack = [{ id: root, edges: edges(nodes.get(root)!), next: 0 }];
    colors.set(root, "visiting");
    while (stack.length > 0) {
      const frame = stack[stack.length - 1];
      if (frame.next === frame.edges.length) {
        colors.set(frame.id, "done");
        stack.pop();
        continue;
      }
      const target = frame.edges[frame.next++];
      const targetNode = nodes.get(target);
      if (!targetNode) continue;
      if (colors.get(target) === "visiting") {
        diagnostics.push({ code, nodeId: frame.id, relatedNodeId: target });
      } else if (!colors.has(target)) {
        colors.set(target, "visiting");
        stack.push({ id: target, edges: edges(targetNode), next: 0 });
      }
    }
  }
}

/** Compare a runtime fact independently of the cursor attached by a stream. */
export function explainAnalyzeFactFingerprint(value: unknown): string | null {
  if (!isExplainAnalyzeEventV1(value)) return null;
  return validatedFactFingerprint(value);
}

function validatedFactFingerprint(value: ExplainAnalyzeEventV1): string {
  // The durable stream cursor belongs to transport, not to event identity.
  // A replay adds `index`; the same fact must remain idempotent. Rust's typed
  // wire decoder also canonicalizes an omitted dependency list to an empty
  // array, so both spellings of that no-edge fact share one fingerprint.
  const canonicalFact: Record<string, unknown> = { ...value };
  delete canonicalFact.index;
  if (
    Array.isArray(canonicalFact.dependency_node_ids) &&
    canonicalFact.dependency_node_ids.length === 0
  ) {
    delete canonicalFact.dependency_node_ids;
  }
  return stableJson(canonicalFact);
}

export function explainAnalyzeMaxConcurrency(
  graph: ExplainAnalyzeGraphV1,
): number | null {
  if (graph.integrity === "unknown" || graph.nodes.some((node) => !node.terminalObserved)) return null;
  const parentIds = new Set(
    graph.nodes.flatMap((node) =>
      node.parentNodeId ? [`${node.clockDomainId}\u0000${node.parentNodeId}`] : [],
    ),
  );
  const leaves = graph.nodes.filter(
    (node) => !parentIds.has(`${node.clockDomainId}\u0000${node.nodeId}`),
  );
  const byClock = new Map<string, Array<{ at: number; delta: number }>>();
  for (const node of leaves) {
    if (node.kind === "admission" || node.kind === "wait") continue;
    const end =
      node.endElapsedMs ??
      (node.durationMs !== undefined ? node.startElapsedMs + node.durationMs : undefined);
    if (end === undefined || end <= node.startElapsedMs) continue;
    const points = byClock.get(node.clockDomainId) ?? [];
    points.push({ at: node.startElapsedMs, delta: 1 }, { at: end, delta: -1 });
    byClock.set(node.clockDomainId, points);
  }
  if (byClock.size === 0) return null;
  let maximum = 0;
  for (const points of byClock.values()) {
    points.sort((left, right) => left.at - right.at || left.delta - right.delta);
    let active = 0;
    for (const point of points) {
      active += point.delta;
      maximum = Math.max(maximum, active);
    }
  }
  return maximum;
}

/** Aggregate observed turn outcomes without imposing order across independent clocks. */
export function explainAnalyzeTurnOutcome(nodes: readonly ExplainAnalyzeNodeV1[]): string | undefined {
  const outcomes = new Set(nodes.filter((node) => node.kind === "turn" && node.terminalObserved)
    .map((node) => {
      switch (node.outcome) {
        case "completed": case "succeeded": case "resolved": return "Complete";
        case "failed": case "rejected": return "Failed";
        case "waiting": case "blocked": case "deferred": return "Waiting";
        default: return node.outcome ? humanOutcome(node.outcome) : "Not recorded";
      }
    }));
  return outcomes.size > 1 ? "Mixed outcomes" : outcomes.values().next().value;
}

/**
 * Produce a self-contained, script-free HTML view of the same graph facts.
 *
 * The saved report opens on the compact text tree because it is useful in a
 * terminal, a pull request, and a browser at the same time. Graph and
 * timeline renderings remain available as native disclosure sections below
 * it; neither view invents a relationship or a clock alignment.
 */
export function renderExplainAnalyzeHtml(
  events: readonly unknown[],
  options: { degraded?: boolean; title?: string } = {},
): string {
  const graph = reduceExplainAnalyzeEvents(events);
  const title = escapeHtml(options.title?.trim() || "Explain Analyze");
  const byClock = new Map<string, ExplainAnalyzeNodeV1[]>();
  const nodeById = new Map(graph.nodes.map((node) => [node.nodeId, node]));
  for (const node of graph.nodes) {
    const group = byClock.get(node.clockDomainId) ?? [];
    group.push(node);
    byClock.set(node.clockDomainId, group);
  }
  const clockDomains = [...byClock.entries()];
  const tree = clockDomains
    .map(([clockDomainId, nodes], index) => renderHtmlTextTreeDomain(nodes, index, clockDomains.length, clockDomainId, nodeById))
    .join("");
  const timeline = clockDomains
    .map(([, nodes], index) => renderHtmlTimelineDomain(nodes, index, clockDomains.length, nodeById))
    .join("");
  const nodeGraph = renderHtmlNodeGraph(graph);
  const graphDetails = renderHtmlGraphDetails(graph);
  const plainTree = renderExplainAnalyzeText(events, { degraded: options.degraded });
  const auxiliaryDetails = explainAnalyzeAuxiliaryDetailsLines(graph);
  const providerAttempts = graph.nodes.filter(
    (node) => node.kind === "provider_attempt",
  );
  const reportedAttempts = providerAttempts.filter((node) => node.terminalObserved && !node.conflicted && node.usage !== undefined);
  const turnDurations = graph.nodes
    .filter((node) => node.kind === "turn" && node.terminalObserved)
    .map((node) => node.durationMs)
    .filter((duration): duration is number => duration !== undefined);
  const turnDuration = turnDurations.length > 0 ? Math.max(...turnDurations) : undefined;
  const tokenTotals = summarizeUsageLanes(reportedAttempts);
  const completedWaits = graph.nodes.filter(
    (node) => node.kind === "wait" && node.terminalObserved && !node.conflicted && node.durationMs !== undefined,
  );
  const openWaitCount = graph.nodes.filter(
    (node) => node.kind === "wait" && !node.terminalObserved && !node.conflicted,
  ).length;
  const waitMsTotal = completedWaits.reduce((total, node) => total + (node.durationMs ?? 0), 0);
  const waitTotalIsSafe = Number.isSafeInteger(waitMsTotal);
  const activeTurn = graph.nodes.some((node) => node.kind === "turn" && !node.terminalObserved);
  const hasConflict = graph.conflictedNodeIds.length > 0;
  const isDegraded = options.degraded || graph.integrity === "unknown" || hasConflict;
  const warning =
    isDegraded
      ? `<aside class="warning" role="status"><span class="warning-icon" aria-hidden="true">!</span><span><strong>Some execution facts are missing or conflict.</strong><br>Refresh this run's saved event history to repair the graph.</span></aside>`
      : "";
  const concurrency = isDegraded ? null : explainAnalyzeMaxConcurrency(graph);
  const status = activeTurn ? "waiting" : turnDurations.length > 0
    ? (graph.nodes.find((node) => node.kind === "turn" && node.terminalObserved)?.outcome ?? "completed")
    : "unavailable";
  const statusLabel = isDegraded ? "Incomplete" : activeTurn ? "Open at capture" : explainAnalyzeTurnOutcome(graph.nodes) ?? "Not recorded";
  const timedCount = graph.nodes.filter((node) => node.durationMs !== undefined).length;
  const tokenSummary = renderHtmlTokenSummary(tokenTotals, reportedAttempts.length, providerAttempts.length);
  const measuredOverlap = concurrency === null
    ? "Overlap not recorded"
    : graph.coverageGaps.length > 0
      ? `Observed overlap · at least ${concurrency} overlapping recorded spans`
      : `Observed overlap · ${concurrency} overlapping recorded spans at peak`;
  const timingSummary = graph.nodes.length === 0
    ? "No execution facts were captured"
    : `Explain Analyze · recorded · ${graph.nodes.length} stages · ${timedCount}/${graph.nodes.length} timed spans · ${clockDomains.length} clock ${clockDomains.length === 1 ? "domain" : "domains"}`;
  const waitSummary = completedWaits.length > 0
    ? `Measured wait time · ${waitTotalIsSafe ? formatMs(waitMsTotal) : "out of range"} across ${completedWaits.length} explicit interval${completedWaits.length === 1 ? "" : "s"}${openWaitCount > 0 ? ` · ${openWaitCount} open at capture` : ""}; overlaps may add`
    : openWaitCount > 0
      ? `Measured wait time · not recorded for ${openWaitCount} open interval${openWaitCount === 1 ? "" : "s"}`
      : "";
  const coverageSummary = graph.coverageGaps.length > 0
    ? `Not timed separately · ${graph.coverageGaps.map(explainAnalyzeCoverageGapLabel).join(" · ")}`
    : "";
  const statusClass = isDegraded || statusLabel === "Mixed outcomes" ? "state-running" : status === "failed" || status === "interrupted" ? "state-failed" : status === "waiting" ? "state-running" : "state-complete";
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="dark"><title>${title} · Explain Analyze</title><style>${EXPLAIN_ANALYZE_HTML_STYLE}</style></head><body><main class="shell"><div class="topline"><div class="brand"><span class="brand-mark" aria-hidden="true">A</span><span>ASTRA <b>/</b> Explain Analyze</span></div><div class="top-actions"><a href="#plain-text-tree">Copy</a><a href="#secondary-graph">Graph</a></div></div><header class="report-head"><div><p class="eyebrow">Explain Analyze</p><h1>${title}</h1><p class="subtitle">What ran, when it ran, and which measurements are available.</p></div><div class="report-result"><strong>${turnDuration === undefined ? "Not recorded" : escapeHtml(formatMs(turnDuration))}</strong><span class="state ${statusClass}">${escapeHtml(statusLabel)}</span></div></header><p class="report-facts">${escapeHtml(timingSummary)}</p><p class="report-facts">${escapeHtml(measuredOverlap)}</p>${coverageSummary ? `<p class="report-facts report-facts-warning">${escapeHtml(coverageSummary)}</p>` : ""}<p class="token-summary">${escapeHtml(tokenSummary)}</p>${explainAnalyzeAuxiliaryUsageLines(graph).length ? `<section class="panel"><h2>Auxiliary model usage</h2>${explainAnalyzeAuxiliaryUsageLines(graph).map(line => `<p>${escapeHtml(line)}</p>`).join("")}</section>` : ""}${auxiliaryDetails.length ? `<section class="panel"><h2>Auxiliary execution details</h2>${auxiliaryDetails.map(line => `<p>${escapeHtml(line)}</p>`).join("")}</section>` : ""}${waitSummary ? `<p class="wait-summary">${escapeHtml(waitSummary)}</p>` : ""}${warning}<section class="tree-panel" id="tree-view" aria-labelledby="tree-heading"><div class="tree-heading"><div><h2 id="tree-heading">Execution tree</h2><p>Recorded containment is shown with branches. Open a group to inspect its children.</p></div><span class="tree-search-hint">Find a stage with Ctrl/Cmd+F</span></div><div class="tree-actions"><a href="#plain-text-tree">Copy plain-text tree</a><span>Use Tab and Enter on groups to expand or collapse.</span></div><div class="text-tree">${tree || "<p class=\"empty\">No execution facts were captured.</p>"}</div></section><section class="copy-panel" id="plain-text-tree"><h2>Copy plain-text tree</h2><textarea readonly aria-label="Copyable plain-text execution tree" rows="${Math.max(4, Math.min(24, graph.nodes.length + clockDomains.length + 2))}">${escapeHtml(plainTree)}</textarea><p>Select the text and copy it; this report is a script-free snapshot.</p></section><section class="secondary-views" aria-label="Secondary Explain Analyze views"><details class="secondary-view" id="secondary-graph"><summary><span>Graph view</span><small>Explicit parent and dependency edges</small></summary><div class="graph-node-view">${nodeGraph}${graphDetails}</div></details><details class="secondary-view" id="secondary-timeline"><summary><span>Timeline view</span><small>Measured spans by clock domain</small></summary><div class="graph-scroll">${timeline || "<div class=\"empty\">No execution facts were captured.</div>"}</div></details></section><footer class="footer"><span>Amber intervals are measured waits. Tool I/O wait is shown only when separately recorded.</span><strong>Saved report · script-free snapshot</strong></footer></main></body></html>`;
}

function renderHtmlTokenSummary(
  tokenTotals: readonly { label: string; value: string | null; knownCount: number }[],
  reportedCount: number,
  requestCount: number,
): string {
  if (reportedCount === 0) return "Provider tokens · not recorded";
  const labels = ["in", "cache read", "cache write", "out"];
  const lanes = tokenTotals.map(({ value, knownCount }, index) =>
    `${labels[index] ?? "tokens"} ${value ?? "unknown"}${value !== null && knownCount < reportedCount ? ` (${knownCount}/${reportedCount})` : ""}`,
  );
  const coverage = reportedCount < requestCount
    ? ` · ${reportedCount}/${requestCount} requests reported`
    : "";
  return `Provider tokens · ${lanes.join(" · ")}${coverage}`;
}

function renderHtmlTextTreeDomain(
  nodes: readonly ExplainAnalyzeNodeV1[],
  domainIndex: number,
  domainCount: number,
  clockDomainId: string,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): string {
  const timelineNodes = new Map(nodes.map((node) => [node.nodeId, node]));
  const children = htmlChildrenByParent(nodes, timelineNodes);
  const visited = new Set<string>();
  const roots = nodes
    .filter((node) => !node.parentNodeId || !timelineNodes.has(node.parentNodeId))
    .sort(compareHtmlNodes);
  const tree = roots
    .map((node, index) => renderHtmlTextTreeNode(
      node,
      children,
      nodeById,
      visited,
      0,
      [],
      index === roots.length - 1,
    ))
    .join("");
  const remaining = nodes
    .filter((node) => !visited.has(node.nodeId))
    .sort(compareHtmlNodes)
    .map((node) => renderHtmlTextTreeNode(node, children, nodeById, visited, 0, [], true))
    .join("");
  const label = domainCount > 1
    ? `Clock domain ${domainIndex + 1} · ${clockDomainId}`
    : `Clock domain · ${clockDomainId}`;
  return `<section class="text-tree-domain" aria-label="${escapeHtml(label)}"><h3>${escapeHtml(label)}</h3>${tree}${remaining}</section>`;
}

function renderHtmlTextTreeNode(
  node: ExplainAnalyzeNodeV1,
  childrenByParent: ReadonlyMap<string, readonly ExplainAnalyzeNodeV1[]>,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  visited: Set<string>,
  depth: number,
  ancestorLast: readonly boolean[],
  isLast: boolean,
): string {
  if (visited.has(node.nodeId)) return "";
  visited.add(node.nodeId);
  const children = [...(childrenByParent.get(node.nodeId) ?? [])]
    .filter((child) => !visited.has(child.nodeId))
    .sort(compareHtmlNodes);
  const guide = depth === 0 ? "" : asciiTreeGuide(ancestorLast, isLast);
  const context = node.context ? renderHtmlInlineContext(node.context) : "";
  const dependencySummary = renderHtmlDependencySummary(node, nodeById);
  const row = renderHtmlTextTreeRow(node, guide, children.length > 0, children.length);
  if (children.length === 0) {
    return `<div class="text-tree-leaf">${row}${dependencySummary}${context}</div>`;
  }
  const childAncestors = depth === 0 ? ancestorLast : [...ancestorLast, isLast];
  const childRows = children
    .map((child, index) => renderHtmlTextTreeNode(
      child,
      childrenByParent,
      nodeById,
      visited,
      depth + 1,
      childAncestors,
      index === children.length - 1,
    ))
    .join("");
  return `<details class="text-tree-group" open><summary>${row}</summary>${dependencySummary}${context}<div class="text-tree-children">${childRows}</div></details>`;
}

function renderHtmlTextTreeRow(
  node: ExplainAnalyzeNodeV1,
  guide: string,
  hasChildren: boolean,
  childCount: number,
): string {
  const status = nodeStatus(node);
  const details = htmlNodeDetails(node);
  const toggle = hasChildren
    ? `<span class="tree-disclosure" aria-hidden="true" title="${childCount} ${childCount === 1 ? "child" : "children"}">▾</span>`
    : `<span class="tree-leaf-mark status-${status.className}" aria-hidden="true"></span>`;
  const wrapper = hasChildren ? "span" : "div";
  const rowClass = `tree-row tree-status-${status.className}${node.kind === "wait" ? " tree-row-wait" : ""}`;
  return `<${wrapper} class="${rowClass}" aria-label="${escapeHtml(`${node.label}, ${details}`)}"><span class="tree-guide" aria-hidden="true">${escapeHtml(guide)}</span>${toggle}<span class="tree-label" title="${escapeHtml(node.label)}">${escapeHtml(node.label)}</span><span class="tree-details">${escapeHtml(details)}</span></${wrapper}>`;
}

function htmlNodeDetails(node: ExplainAnalyzeNodeV1): string {
  const status = nodeStatus(node);
  const duration = node.durationMs === undefined ? "duration not recorded" : formatMs(node.durationMs);
  const request = node.kind === "provider_attempt" ? requestIdentity(node) : "";
  const usage = node.usage
    ? formatUsage(node.usage)
    : node.context
      ? formatExplainAnalyzeContext(node.context)
      : "";
  return [duration, status.label, request, usage].filter(Boolean).join(" · ");
}

function renderHtmlDependencySummary(
  node: ExplainAnalyzeNodeV1,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): string {
  if (node.dependencyNodeIds.length === 0) return "";
  const dependencies = node.dependencyNodeIds.slice(0, 3).map((dependencyId) => {
    const dependency = nodeById.get(dependencyId);
    if (!dependency) return dependencyId;
    return dependency.kind === "provider_attempt"
      ? `${dependency.label} (${requestIdentity(dependency)})`
      : dependency.label;
  });
  const suffix = node.dependencyNodeIds.length > dependencies.length
    ? ` and ${node.dependencyNodeIds.length - dependencies.length} more`
    : "";
  return `<div class="tree-dependencies">after ${dependencies.map((dependency) => `“${escapeHtml(dependency)}”`).join(", ")}${suffix}</div>`;
}

function renderHtmlInlineContext(context: ExplainAnalyzeContextMetricsV1): string {
  return explainAnalyzeContextSections(context).map((section) =>
    `<details class="inline-explanation"><summary>${escapeHtml(section.title)}</summary><p>${escapeHtml(section.description)}</p><dl>${section.rows.map((row) => `<dt>${escapeHtml(row.label)}</dt><dd>${escapeHtml(row.value)}</dd>`).join("")}</dl></details>`,
  ).join("");
}

function htmlChildrenByParent(
  nodes: readonly ExplainAnalyzeNodeV1[],
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): Map<string, ExplainAnalyzeNodeV1[]> {
  const children = new Map<string, ExplainAnalyzeNodeV1[]>();
  for (const node of nodes) {
    if (!node.parentNodeId) continue;
    const parent = nodeById.get(node.parentNodeId);
    if (!parent || parent.clockDomainId !== node.clockDomainId) continue;
    const group = children.get(parent.nodeId) ?? [];
    group.push(node);
    children.set(parent.nodeId, group);
  }
  for (const group of children.values()) group.sort(compareHtmlNodes);
  return children;
}

function compareHtmlNodes(left: ExplainAnalyzeNodeV1, right: ExplainAnalyzeNodeV1) {
  return left.startElapsedMs - right.startElapsedMs || left.nodeId.localeCompare(right.nodeId);
}

function asciiTreeGuide(ancestorLast: readonly boolean[], isLast: boolean): string {
  return `${ancestorLast.map((last) => last ? "   " : "│  ").join("")}${isLast ? "└─ " : "├─ "}`;
}

function renderHtmlNodeGraph(graph: ExplainAnalyzeGraphV1): string {
  const nodeById = new Map(graph.nodes.map((node) => [node.nodeId, node]));
  const detailIds = new Map(graph.nodes.map((node, index) => [node.nodeId, `report-stage-${index}`]));
  const domains = layoutExplainAnalyzeGraph(graph.nodes);
  const visibleCount = domains.reduce((total, domain) => total + domain.nodes.length, 0);
  const notice = visibleCount < graph.nodes.length ? `<p class="dag-limit">Showing ${visibleCount} of ${graph.nodes.length} stages. Switch to Tree for the full recorded hierarchy.</p>` : "";
  return notice + domains.map((domain, domainIndex) => {
    const markerId = `graph-arrow-${domainIndex}`;
    const edges = domain.edges.map((edge) => `<path class="dag-edge dag-edge-${edge.kind}" d="${escapeHtml(edge.path)}"${edge.kind === "dependency" ? ` marker-end="url(#${markerId})"` : ""}/>`).join("");
    const cards = domain.nodes.map((position) => {
      const node = nodeById.get(position.nodeId);
      if (!node) return "";
      const status = nodeStatus(node);
      const kind = node.kind.replace(/_/g, "-");
      const duration = node.durationMs === undefined ? "Not recorded" : formatMs(node.durationMs);
      const usage = node.usage ? formatUsage(node.usage) : node.context ? formatExplainAnalyzeContext(node.context) : "";
      return `<a class="dag-card dag-kind-${kind} dag-status-${status.className}" href="#${detailIds.get(node.nodeId)}" style="left:${position.x}px;top:${position.y}px;width:${position.width}px;height:${position.height}px" aria-label="Inspect ${escapeHtml(node.label)}, ${escapeHtml(status.label)}, ${duration}"><span class="dag-card-head"><i aria-hidden="true"></i><span>${escapeHtml(status.label)}</span><b>${duration}</b></span><strong class="dag-card-title">${escapeHtml(node.label)}</strong>${usage ? `<span class="dag-card-usage" title="${escapeHtml(usage)}">${escapeHtml(usage)}</span>` : `<span class="dag-card-usage">${escapeHtml(formatMs(node.startElapsedMs))} → ${node.endElapsedMs === undefined ? "End not recorded" : escapeHtml(formatMs(node.endElapsedMs))}</span>`}</a>`;
    }).join("");
    return `<section class="dag-domain" aria-label="Execution graph timeline ${domainIndex + 1}"><div class="dag-domain-head"><strong>Timeline ${domainIndex + 1}</strong><span><i class="dag-key-parent"></i> Nested stage <i class="dag-key-dependency"></i> Runs after</span></div><div class="dag-scroll"><div class="dag-canvas" style="width:${domain.width}px;height:${domain.height}px"><svg width="${domain.width}" height="${domain.height}" aria-hidden="true"><defs><marker id="${markerId}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="5" markerHeight="5" orient="auto"><path d="M 0 0 L 10 5 L 0 10 z" fill="#7376df"/></marker></defs>${edges}</svg>${cards}</div></div></section>`;
  }).join("");
}

function renderHtmlGraphDetails(graph: ExplainAnalyzeGraphV1): string {
  return graph.nodes.map((node, index) => {
    const status = nodeStatus(node);
    const usage = node.usage ? `<p>${escapeHtml(formatUsageDetail(node.usage))}</p>` : "";
    const context = node.context ? explainAnalyzeContextSections(node.context).map((section) => `<h4>${escapeHtml(section.title)}</h4><p>${escapeHtml(section.description)}</p><dl>${section.rows.map((row) => `<dt>${escapeHtml(row.label)}</dt><dd>${escapeHtml(row.value)}</dd>`).join("")}</dl>`).join("") : "";
    return `<section id="report-stage-${index}" class="dag-inspection" tabindex="-1"><a href="#secondary-graph" class="dag-close">Close details</a><h3>${escapeHtml(node.label)}</h3><p>${escapeHtml(status.label)} · ${formatMs(node.startElapsedMs)} → ${node.endElapsedMs === undefined ? "End not recorded" : formatMs(node.endElapsedMs)} · ${node.durationMs === undefined ? "Duration not recorded" : formatMs(node.durationMs)}</p>${usage}${context}</section>`;
  }).join("");
}

function renderHtmlTimelineDomain(
  nodes: readonly ExplainAnalyzeNodeV1[],
  domainIndex: number,
  domainCount: number,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): string {
  const domainEnd = Math.max(
    1,
    ...nodes.map((node) => node.endElapsedMs ?? node.startElapsedMs + (node.durationMs ?? 0)),
  );
  const timelineNodes = new Map(nodes.map((node) => [node.nodeId, node]));
  const children = new Map<string, ExplainAnalyzeNodeV1[]>();
  for (const node of nodes) {
    if (!node.parentNodeId || !timelineNodes.has(node.parentNodeId)) continue;
    const group = children.get(node.parentNodeId) ?? [];
    group.push(node);
    children.set(node.parentNodeId, group);
  }
  const visited = new Set<string>();
  const tree = nodes
    .filter((node) => !node.parentNodeId || !timelineNodes.has(node.parentNodeId))
    .map((node) => renderHtmlTreeNode(node, children, nodeById, visited, domainEnd))
    .join("");
  const remaining = nodes
    .filter((node) => !visited.has(node.nodeId))
    .map((node) => renderHtmlTreeNode(node, children, nodeById, visited, domainEnd))
    .join("");
  const ticks = [0, 25, 50, 75, 100]
    .map((percent) => `<span class="axis-tick" style="left:${percent}%">${formatMs(domainEnd * percent / 100)}</span>`)
    .join("");
  const domainLabel = domainCount > 1 ? `Timeline ${domainIndex + 1}` : "Turn timeline";
  return `<section class="domain" aria-label="${domainLabel}"><h3 class="domain-label">${domainLabel}</h3><div class="timeline"><div class="axis-row"><span class="axis-side">Stage</span><div class="axis-track">${ticks}</div><span class="axis-side interval-col">Interval</span><span class="axis-side duration-col">Time</span></div><div class="graph-tree">${tree}${remaining}</div></div></section>`;
}

function renderHtmlTreeNode(
  node: ExplainAnalyzeNodeV1,
  childrenByParent: ReadonlyMap<string, readonly ExplainAnalyzeNodeV1[]>,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  visited: Set<string>,
  domainEnd: number,
  depth = 0,
): string {
  if (visited.has(node.nodeId)) return "";
  visited.add(node.nodeId);
  const children = (childrenByParent.get(node.nodeId) ?? []).filter(
    (child) => !visited.has(child.nodeId),
  );
  const row = renderHtmlNode(node, domainEnd, nodeById, children.length, depth);
  const contextDetails = node.context ? explainAnalyzeContextSections(node.context).map((section) =>
    `<details class="context-details"><summary>${escapeHtml(section.title)}</summary><p>${escapeHtml(section.description)}</p><dl>${section.rows.map((item) => `<dt>${escapeHtml(item.label)}</dt><dd>${escapeHtml(item.value)}</dd>`).join("")}</dl></details>`).join("") : "";
  if (children.length === 0) return row + contextDetails;
  return `<details class="node-group" open>${row}${contextDetails}<div class="node-children" style="--child-indent:${Math.min(depth, 8) * 24}px">${children.map((child) => renderHtmlTreeNode(child, childrenByParent, nodeById, visited, domainEnd, depth + 1)).join("")}</div></details>`;
}

function renderHtmlNode(
  node: ExplainAnalyzeNodeV1,
  domainEnd: number,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
  childCount: number,
  depth: number,
): string {
  const numericStart = Math.max(0, Math.min(node.startElapsedMs, domainEnd));
  const end = Math.max(numericStart, Math.min(node.endElapsedMs ?? numericStart + (node.durationMs ?? 0), domainEnd));
  const left = (numericStart / domainEnd) * 100;
  const width = Math.max(((end - numericStart) / domainEnd) * 100, 0.4);
  const status = nodeStatus(node);
  const duration = node.durationMs === undefined ? "Not recorded" : formatMs(node.durationMs);
  const request = node.kind === "provider_attempt"
    ? `<span class="request-chip">${escapeHtml(requestIdentity(node))}</span>`
    : "";
  const usage = node.usage
    ? `<span class="node-usage" title="${escapeHtml(formatUsageDetail(node.usage))}">${escapeHtml(formatUsage(node.usage))}</span>`
    : "";
  const children = childCount > 0
    ? `<span class="toggle" aria-hidden="true">›</span><span class="tree-count">${childCount} ${childCount === 1 ? "stage" : "stages"}</span>`
    : `<span class="node-leaf-mark status-${status.className}" aria-hidden="true"></span>`;
  const dependencies = node.dependencyNodeIds
    .slice(0, 3)
    .map((dependencyId) => {
      const dependency = nodeById.get(dependencyId);
      if (!dependency) return escapeHtml(dependencyId);
      const request = dependency.kind === "provider_attempt"
        ? ` (${requestIdentity(dependency)})`
        : "";
      return escapeHtml(`${dependency.label}${request}`);
    });
  const dependencySummary = dependencies.length > 0
    ? `<div class="node-dependencies">After ${dependencies.map((label) => `“${label}”`).join(", ")}${node.dependencyNodeIds.length > dependencies.length ? ` and ${node.dependencyNodeIds.length - dependencies.length} more` : ""}</div>`
    : "";
  const start = formatMs(node.startElapsedMs);
  const endTime = node.endElapsedMs === undefined ? "Not recorded" : formatMs(node.endElapsedMs);
  const rowTag = childCount > 0 ? "summary" : "div";
  const kindClass = node.kind.replace(/_/g, "-");
  return `<${rowTag} class="graph-row kind-${kindClass} status-${status.className}${childCount > 0 ? " is-group" : ""}"><div class="node-copy${depth > 0 ? " nested" : ""}" style="--depth:${Math.min(depth, 8)};--indent:${Math.min(depth, 8) * 24}px"><div class="node-titleline">${children}<span class="node-title" title="${escapeHtml(node.label)}">${escapeHtml(node.label)}</span>${request}</div><div class="node-meta"><span class="node-status">${escapeHtml(status.label)}</span>${usage}</div>${dependencySummary}</div><div class="plot" aria-hidden="true"><span class="bar kind-${kindClass}${childCount > 0 ? " is-group" : ""} status-${status.className}" style="left:${left.toFixed(3)}%;width:${node.endElapsedMs === undefined ? "2px" : `${width.toFixed(3)}%`}"></span></div><span class="interval interval-col">${start} – ${endTime}</span><strong class="duration duration-col">${duration}</strong></${rowTag}>`;
}

function nodeStatus(node: ExplainAnalyzeNodeV1): { label: string; className: string } {
  if (!node.terminalObserved && !node.conflicted) return { label: "End not recorded", className: "waiting" };
  if (node.conflicted) return { label: "Conflicting facts", className: "conflicted" };
  if (!node.terminalObserved) return { label: "In progress", className: "running" };
  const outcome = node.outcome;
  if (outcome === "failed" || outcome === "interrupted") {
    return { label: humanOutcome(outcome), className: "failed" };
  }
  if (outcome === "waiting" || outcome === "blocked" || outcome === "deferred") {
    return { label: humanOutcome(outcome), className: "waiting" };
  }
  if (outcome === "succeeded" || outcome === "completed" || outcome === "resolved") {
    return { label: "Completed", className: "complete" };
  }
  return { label: humanOutcome(outcome ?? "unavailable"), className: "complete" };
}

function requestIdentity(node: ExplainAnalyzeNodeV1): string {
  const round = node.roundIndex === undefined ? null : node.roundIndex + 1;
  const attempt = node.attemptIndex === undefined ? null : node.attemptIndex + 1;
  if (round === null && attempt === null) return "Model request";
  if (round !== null && attempt !== null) return `Round ${round} · request ${attempt}`;
  return round !== null ? `Round ${round}` : `Request ${attempt}`;
}

function nodeDepth(
  node: ExplainAnalyzeNodeV1,
  nodeById: ReadonlyMap<string, ExplainAnalyzeNodeV1>,
): number {
  const seen = new Set([node.nodeId]);
  let current = node;
  let depth = 0;
  while (current.parentNodeId && depth < 8) {
    const parent = nodeById.get(current.parentNodeId);
    if (!parent || parent.clockDomainId !== node.clockDomainId || seen.has(parent.nodeId)) break;
    seen.add(parent.nodeId);
    current = parent;
    depth += 1;
  }
  return depth;
}

export function explainAnalyzeNodeIsActive(node: ExplainAnalyzeNodeV1): boolean {
  return !node.terminalObserved;
}

export function formatMs(ms: number): string {
  if (ms < 1_000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(Math.round(ms / 100) / 10).toFixed(1)} s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1_000);
  return seconds === 0 ? `${minutes} min` : `${minutes} min ${seconds} s`;
}

export function formatUsage(usage: ExplainAnalyzeUsageV1): string {
  const lanes = [
    ["in", usage.fresh_input_tokens],
    ["cache", usage.cache_read_tokens],
    ["write", usage.cache_creation_tokens],
    ["out", usage.output_tokens],
  ] as const;
  const details = lanes
    .flatMap(([name, count]) =>
      count === undefined ? [] : [`${name} ${count.toLocaleString()}`],
    );
  return `${details.join(" · ")}${usage.basis === "provider_partial" ? " · partial" : usage.basis === "runtime_estimated" ? " · estimated" : ""}`;
}

export function formatUsageDetail(usage: ExplainAnalyzeUsageV1): string {
  const lanes = [
    ["Fresh input", usage.fresh_input_tokens],
    ["Cache read", usage.cache_read_tokens],
    ["Cache creation", usage.cache_creation_tokens],
    ["Output", usage.output_tokens],
  ] as const;
  const details = lanes
    .flatMap(([name, count]) =>
      count === undefined ? [] : [`${name}: ${count.toLocaleString()}`],
    )
    .join(" · ");
  const basis = usage.basis === "provider_exact"
    ? "Provider reported"
    : usage.basis === "provider_partial"
      ? "Partial provider report"
      : "Runtime estimate";
  return `${details} (${basis})`;
}

function summarizeUsageLanes(
  nodes: readonly ExplainAnalyzeNodeV1[],
): Array<{ label: string; value: string | null; knownCount: number }> {
  const lanes = [
    ["Fresh input", "fresh_input_tokens"],
    ["Cache read", "cache_read_tokens"],
    ["Cache created", "cache_creation_tokens"],
    ["Output", "output_tokens"],
  ] as const;
  return lanes.map(([label, key]) => {
    const values = nodes.map((node) => node.usage?.[key]);
    const knownValues = values.filter((lane): lane is number => lane !== undefined);
    const value = knownValues.length > 0
      ? knownValues.reduce((sum, lane) => sum + BigInt(lane), 0n).toLocaleString()
      : null;
    return { label, value, knownCount: knownValues.length };
  });
}

function humanOutcome(outcome: NonNullable<ExplainAnalyzeEventV1["outcome"]>) {
  const labels: Record<typeof outcome, string> = {
    completed: "Completed",
    succeeded: "Succeeded",
    failed: "Failed",
    cancelled: "Cancelled",
    interrupted: "Interrupted",
    blocked: "Blocked",
    waiting: "Waiting",
    rejected: "Rejected",
    reused: "Reused",
    suppressed: "Suppressed",
    deferred: "Deferred",
    resolved: "Resolved",
    fallback: "Fallback used",
    unavailable: "Unavailable",
    delegated: "Delegated",
  };
  return labels[outcome];
}

function sameStrings(left: readonly string[], right: readonly string[]) {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  if (isRecord(value)) {
    const pairs = Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`);
    return `{${pairs.join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown, maxLength: number): value is string {
  return typeof value === "string" && value.trim().length > 0 && value.length <= maxLength;
}

function isNonNegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => {
    switch (character) {
      case "&": return "&amp;";
      case "<": return "&lt;";
      case ">": return "&gt;";
      case '"': return "&quot;";
      default: return "&#39;";
    }
  });
}
