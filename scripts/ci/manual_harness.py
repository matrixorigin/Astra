"""Observe typed safe progress; never parse or publish harness/provider logs."""
import argparse
from collections import Counter
import html
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

SCHEMA = "astra.harness_progress.v8"
MAX_RECORD = 262_144
MAX_FILE = 32 * 1024 * 1024
STATUSES = ("passed", "failed", "cancelled", "unavailable")
# Closed diagnostic vocabulary shared with the canonical Rust verifier.
CHILD_PROOF_CONTRADICTIONS = frozenset("""
child_count_mismatch child_not_completed execution_identity_mismatch model_mismatch
fanout_start_missing fanout_start_repeated fanout_start_unsuccessful fanout_group_mismatch
fanout_rejection_executed fanout_selection_invalid fanout_title_invalid
fanout_child_count_mismatch fanout_label_invalid fanout_identity_mismatch
fanout_children_mismatch spawn_identity_mismatch slot_missing model_ambiguous
child_selection_repeated offering_mismatch thinking_mismatch mutation_mismatch
question_unanswered logical_rounds_mismatch child_result_conflict
expected_result_mismatch result_digest_mismatch finalization_before_adoption
""".split())
CHILD_PROOF_UNAVAILABLE = frozenset("""
run_tree_unavailable root_running root_waiting root_paused root_delegated
root_interrupted root_failed root_cancelled fanout_receipt_unavailable model_identity_unavailable
fanout_rejection_execution_unknown fanout_selection_unavailable
offering_identity_unavailable thinking_unavailable mutation_unavailable
question_evidence_unavailable logical_rounds_unavailable child_result_unavailable
adoption_evidence_unavailable agent_identity_unavailable finalization_unavailable
physical_execution_unavailable result_window_unavailable result_window_identity_conflict
result_window_digest_conflict result_window_invalid
""".split())

METRICS = ("duration_ms", "ttft_ms", "turn_rounds", "tool_calls_count", "prompt_tokens",
           "cached_input_tokens", "cache_creation_tokens", "completion_tokens")
OUTCOME_STATUSES = {"completed", "failed", "rejected", "blocked", "started", "unknown",
                    "waiting", "paused", "cancelled", "interrupted", "timed_out", "incomplete",
                    "completed_with_issues", "timeout", "still_running", "launched", "other", "not_reported"}
ERROR_KINDS = {"rate_limit", "payment_required", "server_error", "auth", "context_window",
               "invalid_request", "contract_violation", "stream_idle", "provider_deadline",
               "stream_transport", "connection_pool_exhausted", "budget_exhausted",
               "tool_rounds_exhausted", "network", "tool_not_found", "tool_invalid_args",
               "policy_denied", "tool_timeout", "tool_unavailable", "tool_binding",
               "tool_outcome_unknown", "resource_limit", "database_error", "stall",
               "missing_model_selection", "cancelled", "unknown"}
OUTCOME_ERROR_KINDS = ERROR_KINDS | {"other", "not_reported",
               "delegation_model_scope_unresolved", "delegation_model_unavailable",
               "delegation_preparation_unavailable", "delegation_model_assessment_unavailable",
               "invalid_delegation_model_scope", "execution_time_budget_exhausted"}
INTERRUPTION_KINDS = frozenset("""
budget_exhausted empty_completion token_budget_exceeded rate_limited cooldown_rejected
user_cancelled context_overflow auth_failure critical_verdict approval_rejected server_overload
stream_transport stream_idle provider_deadline harness_blocked harness_paused interrupted
execution_incomplete executor_dropped timeout cancelled other
""".split())
TERMINAL_ERROR_KINDS = ERROR_KINDS | {"api_error", "tool_failure", "persistence_error",
    "partial", "unfinished", "session_cancellation_pending", "other"}
ERROR_CODES = TERMINAL_ERROR_KINDS | INTERRUPTION_KINDS | {
    "canonical_persistence_failed", "crash_recovery", "genesis_unavailable",
    "terminal_handoff_window_closed", "internal_error", "tool_error", "api_error"}
STREAM_EVENTS = frozenset("""
run_started transcript_committed output_delta terminated_completed terminated_delegated
terminated_failed terminated_interrupted terminated_cancelled gap other
""".split())
JOURNAL_EVENTS = frozenset("""
llm_round turn turn_error tool_invocation llm_round_started llm_round_completed llm_round_failed
 tool_call_started tool_call_completed tool_call_failed tool_call_skipped
step_started step_completed step_incomplete step_failed other
""".split())
DEADLINE_PHASES = frozenset("""
semantic_progress actionable_output provider_work consuming_provider_stream waiting_provider_response
sending_provider_request provider_attempt_admission provider_attempt_terminalization
provider_attempt_persistence other
""".split())
LOOP_FINISH_REASONS = frozenset("""
stop content_filter function_call length tool_calls continue interrupted superseded_by_runtime_input direct_child_completion_wait
runtime_input_arrived_before_settlement textless_response_retry canonical_work_revalidation_unavailable
paused_after_provider_response canonical_work_synthesis_revoked work_settlement_retry
completion_action_no_tool_retry completion_action_incomplete workspace_completion_retry
explicit_verification_retry round_slice_without_typed_completion completion_contract_incomplete token_budget_exceeded
other not_reported
""".split())
CHILD_BOUNDARY_OUTCOMES = frozenset("""
results_staged results_adopted wait_started cancelled readmission_failed synthesis_ready runtime_input_ready
observation_timed_out synthesis_budget_exhausted deadline finalization_accepted finalization_incomplete
finalization_interrupted other not_reported
""".split())


def safe_loop_boundaries(value):
    if not isinstance(value, dict) or set(value) != {"llm_round", "child_barrier"}:
        raise ValueError("invalid loop boundaries")
    for kind, tags in (("llm_round", LOOP_FINISH_REASONS), ("child_barrier", CHILD_BOUNDARY_OUTCOMES)):
        scopes = value[kind]
        if not isinstance(scopes, dict) or not set(scopes) <= {"root", "other_run", "unattributed"}:
            raise ValueError("invalid loop scope")
        for counts in scopes.values():
            if not isinstance(counts, dict) or not set(counts) <= tags:
                raise ValueError("invalid loop codes")
            for count in counts.values():
                integer(count)


def safe_execution_diagnostic(value):
    if (not isinstance(value, dict) or set(value) != {
        "final_state", "interruption_kind", "error_kind", "error_code",
        "server_admission_not_dispatched", "stream", "journal"}):
        raise ValueError("invalid execution diagnostic")
    for key, allowed in (("final_state", {"completed", "failed", "interrupted", "other"}),
                         ("interruption_kind", INTERRUPTION_KINDS),
                         ("error_kind", TERMINAL_ERROR_KINDS), ("error_code", ERROR_CODES)):
        tag = value[key]
        if tag is not None and (not isinstance(tag, str) or tag not in allowed):
            raise ValueError("invalid execution tag")
    if type(value["server_admission_not_dispatched"]) is not bool:
        raise ValueError("invalid admission diagnostic")
    for key, allowed, fields in (
        ("stream", STREAM_EVENTS, {"identity_verified", "has_diagnostics"}),
        ("journal", JOURNAL_EVENTS, {"skipped_lines", "dropped_lines", "integrity_errors"})):
        capture = value[key]
        if capture is None:
            continue
        expected = fields | {"last_event", "event_counts"}
        if key == "journal":
            expected.add("last_provider_failure")
            expected.add("loop_boundaries")
        if not isinstance(capture, dict) or set(capture) != expected:
            raise ValueError("invalid capture diagnostic")
        if key == "stream":
            if any(type(capture[field]) is not bool for field in fields):
                raise ValueError("invalid capture identity")
        else:
            for field in fields:
                integer(capture[field])
            safe_loop_boundaries(capture["loop_boundaries"])
        counts = capture["event_counts"]
        if not isinstance(counts, dict) or not set(counts) <= allowed:
            raise ValueError("invalid event counters")
        for count in counts.values():
            integer(count)
        last = capture["last_event"]
        if last is not None and (not isinstance(last, str) or last not in allowed):
            raise ValueError("invalid last event")
        if key == "journal" and capture["last_provider_failure"] is not None:
            failure = capture["last_provider_failure"]
            if not isinstance(failure, dict) or set(failure) != {
                "event", "run_scope", "error_kind", "failure_stage", "deadline_phase", "dispatch_started", "provider_completed"}:
                raise ValueError("invalid provider failure diagnostic")
            for field, tags in (("event", {"llm_round_failed", "llm_round_completed"}),
                                ("run_scope", {"root", "other_run", "unattributed"}),
                                ("error_kind", TERMINAL_ERROR_KINDS),
                                ("failure_stage", {"admission", "provider", "settlement", "other"}),
                                ("deadline_phase", DEADLINE_PHASES)):
                tag = failure[field]
                if tag is not None and (not isinstance(tag, str) or tag not in tags):
                    raise ValueError("invalid provider failure tag")
            for field in ("dispatch_started", "provider_completed"):
                if failure[field] is not None and type(failure[field]) is not bool:
                    raise ValueError("invalid provider failure flag")

VALIDATION_CODES = frozenset("""
required type const enum min_length max_length minimum maximum unique_items
min_items max_items union additional_properties action_fields required_alternative malformed_json
""".split())


def safe_validation(value):
    if value is None:
        return
    if (not isinstance(value, dict) or set(value) != {"issues", "omitted"}
        or not isinstance(value["issues"], list) or len(value["issues"]) > 8
        or len(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()) > 4096):
        raise ValueError("invalid validation summary")
    integer(value["omitted"])
    for issue in value["issues"]:
        if (not isinstance(issue, dict) or set(issue) != {"code", "path"}
            or not isinstance(issue["code"], str) or issue["code"] not in VALIDATION_CODES):
            raise ValueError("invalid validation issue")
        path = issue["path"]
        if path is not None and (not isinstance(path, str)
            or len(path.encode()) > 256 or (path and not path.startswith("/"))
            or path.count("/") > 8):
            raise ValueError("invalid validation location")


def integer(value):
    if type(value) is not int or not 0 <= value <= (1 << 64) - 1:
        raise ValueError("invalid integer")
    return value


def identity(value):
    if not isinstance(value, str) or not value or len(value) > 4096:
        raise ValueError("invalid identity")
    return value


def unique_fields(pairs):
    fields = {}
    for name, value in pairs:
        if name in fields:
            raise ValueError("duplicate JSON field")
        fields[name] = value
    return fields


def safe_diagnostic(value, depth):
    if value is None:
        return
    kind = value.get("kind")
    fields = {"count": {"min", "max", "actual"}, "exit": {"expected", "actual"},
              "tool_count": {"min", "max", "actual", "considered", "expected_ok", "outcome_matches", "error_matches", "predicate_matches", "predicate_mismatches", "observed_outcomes", "outcomes_omitted"},
              "journal_count": {"min", "max", "actual", "considered", "outcome_matches", "missing", "type_mismatch", "value_mismatch"},
              "journal_ratio": {"min", "min_calls", "allowed_failures", "terminal", "succeeded", "failed", "pending", "adjusted_terminal"},
              "sequence": {"matched", "expected"}, "proof": {"proven"},
              "judge": {"threshold", "assessment", "failure"}, "unavailable": set(), "composite": {"checks"},
              "child_adoption": {"status", "reason"}}
    fields["cache_scope"] = {"scope", "observations", "post_cold_cache_read_tokens", "executions", "executions_omitted"}
    if kind == "child_adoption" and "mutation" in value:
        fields[kind] = fields[kind] | {"mutation"}
    if kind not in fields or set(value) != {"kind"} | fields[kind]:
        raise ValueError("unexpected diagnostic payload")
    if kind == "cache_scope":
        if value["scope"] not in ("conversation_turns", "intra_turn_rounds"):
            raise ValueError("invalid cache scope")
        observations, reads = value["observations"], value["post_cold_cache_read_tokens"]
        if (observations is None) != (reads is None):
            raise ValueError("incomplete cache observation")
        if observations is not None:
            integer(observations)
            if (not isinstance(reads, str) or not 1 <= len(reads) <= 39
                or any(c not in "0123456789" for c in reads) or int(reads) >= 1 << 128):
                raise ValueError("invalid cache read sum")
        integer(value["executions_omitted"])
        if not isinstance(value["executions"], list) or len(value["executions"]) > 16:
            raise ValueError("invalid cache execution evidence")
        for execution in value["executions"]:
            if (not isinstance(execution, dict) or set(execution) != {
                "capture", "run_matches", "session_matches", "scope_count", "cache_read_tokens"}
                or execution["capture"] not in ("missing", "identity_unverified", "capture_diagnostic",
                                                "snapshot_pending", "stream_gap", "verified")):
                raise ValueError("invalid cache capture diagnostic")
            for key in ("run_matches", "session_matches"):
                if execution[key] is not None and type(execution[key]) is not bool:
                    raise ValueError("invalid cache identity diagnostic")
            for key in ("scope_count", "cache_read_tokens"):
                if execution[key] is not None:
                    integer(execution[key])
    elif kind == "journal_count":
        for key in fields[kind]:
            integer(value[key])
        if (value["min"] > value["max"] or value["outcome_matches"] > value["considered"]
            or value["actual"] + value["missing"] + value["type_mismatch"] + value["value_mismatch"] != value["outcome_matches"]):
            raise ValueError("inconsistent journal count accounting")
    elif kind == "journal_ratio":
        for key in fields[kind] - {"min"}:
            integer(value[key])
        if (type(value["min"]) not in (int, float) or not math.isfinite(value["min"])
            or not 0 <= value["min"] <= 1 or value["min_calls"] < 1
            or value["allowed_failures"] >= value["min_calls"]
            or value["succeeded"] + value["failed"] != value["terminal"]
            or value["adjusted_terminal"] != value["succeeded"] + max(0, value["failed"] - value["allowed_failures"])):
            raise ValueError("inconsistent journal ratio accounting")
    elif kind == "composite":
        safe_criteria(value["checks"], owner_passed=False, depth=depth+1)
    elif kind == "child_adoption":
        status, reason = value["status"], value["reason"]
        if not isinstance(status, str):
            raise ValueError("invalid child proof status")
        allowed = {"contradicted": CHILD_PROOF_CONTRADICTIONS,
                   "unavailable": CHILD_PROOF_UNAVAILABLE}
        if status == "proven":
            if reason is not None:
                raise ValueError("proven child proof has a failure reason")
        elif (not isinstance(reason, str) or status not in allowed
              or reason not in allowed[status]):
            raise ValueError("invalid child proof diagnostic")
        if "mutation" in value:
            mutation = value["mutation"]
            intents = {"unknown", "read_only", "may_mutate", "must_mutate"}
            personas = {"omitted", "explore", "code_review", "task", "general_purpose", "custom", "unavailable"}
            if (status != "contradicted" or reason != "mutation_mismatch"
                or not isinstance(mutation, dict) or set(mutation) != {"expected", "actual", "requested_persona"}
                or not isinstance(mutation["expected"], str) or mutation["expected"] not in intents
                or (mutation["actual"] is not None and (not isinstance(mutation["actual"], str) or mutation["actual"] not in intents))
                or not isinstance(mutation["requested_persona"], str) or mutation["requested_persona"] not in personas):
                raise ValueError("invalid mutation observation")
    elif kind == "judge":
        if value["assessment"] not in ("graded", "accepted", "rejected", "unavailable", "unreported"):
            raise ValueError("invalid judge assessment")
        if type(value["threshold"]) not in (int, float) or not math.isfinite(value["threshold"]) or not 0 <= value["threshold"] <= 1:
            raise ValueError("invalid judge threshold")
        failure = value["failure"]
        if (value["assessment"] == "unavailable") != (failure is not None):
            raise ValueError("inconsistent judge failure")
        if failure is not None and (not isinstance(failure, dict)
            or set(failure) != {"error_kind", "stage"}
            or not isinstance(failure["error_kind"], str)
            or failure["error_kind"] not in ERROR_KINDS
            or not isinstance(failure["stage"], str)
            or failure["stage"] not in {"request_preparation", "subprocess", "completion",
                "response_validation", "assessment", "aggregation", "unknown"}):
            raise ValueError("invalid judge failure")
    elif kind == "proof":
        if type(value["proven"]) is not bool:
            raise ValueError("invalid proof")
    elif kind == "exit":
        if any(type(value[k]) is not int or not -(1 << 31) <= value[k] < (1 << 31) for k in fields[kind]):
            raise ValueError("invalid exit diagnostic")
    else:
        for key in fields[kind] - {"expected_ok", "error_matches", "predicate_matches", "predicate_mismatches", "observed_outcomes"}:
            integer(value[key])
        if kind == "tool_count":
            if value["expected_ok"] is not None and type(value["expected_ok"]) is not bool:
                raise ValueError("invalid expected outcome")
            if value["error_matches"] is not None:
                integer(value["error_matches"])
            if not isinstance(value["predicate_matches"], list) or len(value["predicate_matches"]) > 16:
                raise ValueError("invalid predicate counts")
            for count in value["predicate_matches"]:
                integer(count)
            safe_predicate_mismatches(value)
            outcomes = value["observed_outcomes"]
            if (not isinstance(outcomes, list) or len(outcomes) > 16
                or len(json.dumps(outcomes, separators=(",", ":"), ensure_ascii=False).encode()) > 8192):
                raise ValueError("invalid outcome observations")
            observed = value["outcomes_omitted"]
            for entry in outcomes:
                if set(entry) != {"outcome", "count"} or integer(entry["count"]) == 0:
                    raise ValueError("invalid outcome count")
                outcome = entry["outcome"]
                if (set(outcome) != {"disposition", "ok", "status", "error_kind", "execution", "validation"}
                    or (outcome["ok"] is not None and type(outcome["ok"]) is not bool)
                    or outcome["disposition"] not in ("executed", "rejected", "reused", "suppressed", "deferred")
                    or outcome["status"] not in OUTCOME_STATUSES
                    or outcome["error_kind"] not in OUTCOME_ERROR_KINDS
                    or outcome["execution"] not in ("executed", "not_executed", "unknown", "not_reported")):
                    raise ValueError("invalid outcome tags")
                safe_validation(outcome["validation"])
                if outcome["validation"] is not None and outcome["error_kind"] != "tool_invalid_args":
                    raise ValueError("validation summary without argument rejection")
                observed += entry["count"]
            if observed != value["considered"]:
                raise ValueError("incomplete outcome accounting")


def safe_predicate_mismatches(value):
    entries = value["predicate_mismatches"]
    if not isinstance(entries, list) or len(entries) > 16:
        raise ValueError("invalid predicate mismatches")
    seen = set()
    types = {"missing", "null", "boolean", "number", "string", "array", "object"}
    for entry in entries:
        if not isinstance(entry, dict) or set(entry) != {"predicate_index", "failure_count", "first_mismatch"}:
            raise ValueError("invalid predicate mismatch")
        index = integer(entry["predicate_index"])
        if (index >= len(value["predicate_matches"]) or index in seen
            or integer(entry["failure_count"]) == 0
            or entry["failure_count"] != value["outcome_matches"] - value["predicate_matches"][index]):
            raise ValueError("invalid predicate mismatch accounting")
        seen.add(index)
        detail = entry["first_mismatch"]
        if not isinstance(detail, dict):
            raise ValueError("invalid predicate mismatch detail")
        reason = detail.get("reason")
        if reason == "truncated":
            if set(detail) != {"reason", "limit"} or detail["limit"] not in ("path", "structure"):
                raise ValueError("invalid predicate truncation")
            continue
        fields = {"missing": {"expected_type", "actual_type"},
                  "type_mismatch": {"expected_type", "actual_type"},
                  "scalar_mismatch": {"expected_type", "actual_type"},
                  "substring_absent": {"expected_type", "actual_type"},
                  "unexpected_members": {"count"},
                  "array_length_mismatch": {"expected_length", "actual_length"}}
        if not isinstance(reason, str) or reason not in fields or set(detail) != {"reason", "path"} | fields[reason]:
            raise ValueError("unexpected predicate mismatch detail")
        path = detail["path"]
        if (not isinstance(path, str) or len(path.encode()) > 512
            or (path and not path.startswith("/"))):
            raise ValueError("invalid predicate mismatch path")
        for key in fields[reason]:
            if key.endswith("_type"):
                if not isinstance(detail[key], str) or detail[key] not in types:
                    raise ValueError("invalid predicate JSON type")
            else:
                integer(detail[key])
    failed = sum(count < value["outcome_matches"] for count in value["predicate_matches"])
    if len(seen) != failed:
        raise ValueError("incomplete predicate mismatch accounting")


def safe_criteria(items, *, owner_passed, depth=0):
    if depth > 32:
        raise ValueError("excessive diagnostic nesting")
    if not isinstance(items, list):
        raise ValueError("invalid criteria")
    for item in items:
        if set(item) != {"type", "severity", "score", "passed", "evaluation", "diagnostic"}:
            raise ValueError("unexpected criterion payload")
        identity(item["type"])
        safe_diagnostic(item["diagnostic"], depth)
        if (item["severity"] not in ("hard", "soft", "quality")
            or item["evaluation"] not in ("scored", "unscored", "reported")
            or (item["passed"] is not None and type(item["passed"]) is not bool)
            or (item["score"] is not None and
                (type(item["score"]) not in (int, float) or not math.isfinite(item["score"])))):
            raise ValueError("invalid criterion fields")
        # Validate the producer's schema contract; never recompute a grade or threshold.
        advisory = item["type"] == "judger"
        judge = advisory or item["type"] == "hard_judger"
        if judge and item["severity"] != ("quality" if advisory else "hard"):
            raise ValueError("contradictory judge severity")
        unscored = advisory and item["score"] is None
        expected = "scored" if judge and item["score"] is not None else "unscored" if unscored else "reported"
        if item["evaluation"] != expected or (item["passed"] is None) != unscored:
            raise ValueError("contradictory grading provenance")
        if owner_passed and item["severity"] == "hard" and item["passed"] is False:
            raise ValueError("passed owner contradicts required check")
    return items


def check_lines(items, prefix):
    """Explain already-graded facts, never regrade or infer a root cause."""
    for index, check in enumerate(items, 1):
        judge = check["type"] in ("judger", "hard_judger")
        if check["passed"] is not False and not judge:
            continue
        location = f"{prefix}.{index} {check['type']} ({check['severity']})"
        diagnostic = check["diagnostic"] or {}
        kind = diagnostic.get("kind")
        if judge:
            threshold = diagnostic.get("threshold", "unknown")
            assessment = diagnostic.get("assessment")
            if assessment == "unavailable":
                failure = diagnostic["failure"]
                reason = (f"LLM judge unavailable: {failure['error_kind']} at {failure['stage']}; "
                          "this is NOT a negative quality verdict")
            elif assessment in ("accepted", "rejected"):
                reason = f"LLM judge {assessment} the declared criterion (acceptance verdict, not a numeric grade)"
                if assessment == "rejected":
                    reason += "; structured rejection reason was not captured; do not infer one from this verdict"
            elif check["evaluation"] == "scored":
                reason = f"LLM judge score={check['score']}, threshold={threshold}; passed={check['passed']}"
            elif check["evaluation"] == "unscored":
                reason = "LLM judge unscored; no usable score (not a pass)"
            else:
                reason = f"Required judge passed={check['passed']}; decision diagnostics unreported (do not infer a model verdict)"
        elif kind in ("count", "tool_count", "journal_count"):
            reason = f"expected {diagnostic['min']}..{diagnostic['max']}, observed {diagnostic['actual']}"
            if kind == "journal_count":
                reason += (f"; tool/scope candidates={diagnostic['considered']}, "
                           f"matching outcomes={diagnostic['outcome_matches']}; "
                           f"predicate mismatches: missing={diagnostic['missing']}, "
                           f"type={diagnostic['type_mismatch']}, value={diagnostic['value_mismatch']}; "
                           "predicate definition is in the case YAML at this criterion index; raw values withheld")
            if kind == "tool_count":
                reason += (f"; tool/scope candidates={diagnostic['considered']}; "
                           f"expected ok={diagnostic['expected_ok']}, matching outcomes={diagnostic['outcome_matches']}")
                if diagnostic["error_matches"] is not None:
                    reason += f"; error-text matches={diagnostic['error_matches']}"
                reason += f"; independent predicate matches (YAML order)={diagnostic['predicate_matches']}"
                reason += "; all predicates must match the SAME call"
                if diagnostic["predicate_mismatches"]:
                    reason += "; first predicate mismatches=" + json.dumps(diagnostic["predicate_mismatches"], sort_keys=True)
                reason += "; observed outcomes=" + json.dumps(diagnostic["observed_outcomes"], sort_keys=True)
                if diagnostic["outcomes_omitted"]:
                    reason += f"; omitted observations={diagnostic['outcomes_omitted']}"
                kinds = {item["outcome"]["error_kind"] for item in diagnostic["observed_outcomes"]}
                if "tool_invalid_args" in kinds:
                    reason += "; Failure stage: argument validation reported; this does not prove the requested model was checked"
                    summaries = [entry["outcome"]["validation"] for entry in diagnostic["observed_outcomes"]
                                 if entry["outcome"]["validation"] is not None]
                    if not summaries:
                        reason += "; validation rules unavailable in the captured receipt; exact invalid field is unknown"
                    for summary in summaries:
                        reason += "; validation rules=" + ", ".join(
                            f"{issue['code']} at {('<root>' if issue['path'] == '' else issue['path']) if issue['path'] is not None else '<opaque>'}"
                            for issue in summary["issues"])
                        if summary["omitted"]:
                            reason += f"; omitted validation issues={summary['omitted']}"
                elif kinds & {"delegation_model_scope_unresolved", "delegation_model_unavailable", "invalid_delegation_model_scope"}:
                    reason += "; Attribution: delegation model admission error reported; child execution is a separate check"
                elif "invalid_request" in kinds:
                    reason += "; Attribution: invalid request reported; the safe code alone does not identify which input was invalid"
                else:
                    reason += "; Attribution: contract mismatch observed; root cause is not established by these counts"
        elif kind == "journal_ratio":
            reason = (f"success={diagnostic['succeeded']}, failed={diagnostic['failed']}, "
                      f"terminal={diagnostic['terminal']}, pending={diagnostic['pending']}; "
                      f"allowed failures={diagnostic['allowed_failures']}; "
                      f"adjusted ratio={diagnostic['succeeded']}/{diagnostic['adjusted_terminal']} "
                      f"(zero denominator means 0); required ratio>={diagnostic['min']}, "
                      f"terminal calls>={diagnostic['min_calls']}")
        elif kind == "exit":
            reason = f"expected exit={diagnostic['expected']}, observed exit={diagnostic['actual']}"
        elif kind == "cache_scope":
            reason = (f"required scope={diagnostic['scope']}; observations={diagnostic['observations']}; "
                      f"post-cold cache reads={diagnostic['post_cold_cache_read_tokens']}; "
                      "execution evidence=" + json.dumps(diagnostic["executions"], sort_keys=True) +
                      f"; omitted executions={diagnostic['executions_omitted']}")
        elif kind == "sequence":
            reason = f"matched {diagnostic['matched']} of {diagnostic['expected']} ordered tool receipts"
        elif kind == "unavailable":
            reason = "Canonical execution evidence unavailable or incomplete; cannot verify this contract"
        elif kind == "child_adoption":
            reason = f"Child adoption verification {diagnostic['status']}: {diagnostic['reason']}"
            if "mutation" in diagnostic:
                reason += "; mutation observations (not causal attribution)=" + json.dumps(diagnostic["mutation"], sort_keys=True)
        elif kind == "proof":
            reason = f"Canonical child identity/result adoption proven={diagnostic['proven']}"
        elif kind == "composite":
            yield f"{location}: Required combination failed; child severities do not override this group's {check['severity']} requirement"
            yield from check_lines(diagnostic["checks"], f"{prefix}.{index}")
            continue
        else:
            reason = "Condition not met; detailed payload is private (not included in public CI artifacts)"
        yield f"{location}: {reason}"


def row_checks(row):
    lines = list(check_lines(row["criteria"], "root"))
    if row["status"] != "passed":
        lines.append("Execution diagnostics (observations, not causal attribution): " +
                     json.dumps(row["execution_diagnostic"], sort_keys=True))
    if row["token_usage_coverage"] is None:
        lines.append("Provider usage coverage unavailable; terminal-reported zero counters do not prove zero usage or no tool execution.")
    for step in row["steps"]:
        lines.extend(check_lines(step["criteria"], f"step[{step['step_index']}]"))
        if not step["passed"]:
            lines.append(f"step[{step['step_index']}] execution diagnostics: " +
                         json.dumps(step["execution_diagnostic"], sort_keys=True))
    if not any(c["type"] in ("judger", "hard_judger") for checks in
               [row["criteria"], *(s["criteria"] for s in row["steps"])] for c in checks):
        lines.append("LLM judge: not configured for this case; checks are deterministic, not an LLM assessment.")
    return lines


def display_status(row):
    return "passed (warnings)" if row["status"] == "passed" and row["has_warnings"] else row["status"]


def safe_row(row):
    if set(row) != {"case", "model", "run_index", "status", "has_warnings", "failure_class",
                    "exit_code", "criteria", "steps", "token_usage_coverage", "terminal_reported",
                    "execution_diagnostic"}:
        raise ValueError("unexpected row payload")
    identity(row["case"]), identity(row["model"]), integer(row["run_index"])
    if row["status"] not in STATUSES or type(row["has_warnings"]) is not bool or type(row["exit_code"]) is not int:
        raise ValueError("invalid terminal row")
    if row["failure_class"] is not None:
        identity(row["failure_class"])
    safe_criteria(row["criteria"], owner_passed=row["status"] == "passed")
    safe_execution_diagnostic(row["execution_diagnostic"])
    if not isinstance(row["steps"], list) or set(row["terminal_reported"]) != set(METRICS):
        raise ValueError("invalid steps or metrics")
    for value in row["terminal_reported"].values():
        integer(value)
    for step in row["steps"]:
        if set(step) != {"step_index", "passed", "duration_ms", "criteria", "execution_diagnostic"} or type(step["passed"]) is not bool:
            raise ValueError("unexpected step payload")
        integer(step["step_index"]), integer(step["duration_ms"])
        safe_criteria(step["criteria"], owner_passed=step["passed"])
        safe_execution_diagnostic(step["execution_diagnostic"])
    if row["status"] == "passed" and any(not step["passed"] for step in row["steps"]):
        raise ValueError("passed status contradicts canonical checks")
    coverage = row["token_usage_coverage"]
    if coverage is not None:
        if set(coverage) != {"scope", "attempts", "provider_reported", "unavailable", "status"}:
            raise ValueError("unexpected coverage")
        identity(coverage["scope"]), identity(coverage["status"])
        for name in ("attempts", "provider_reported", "unavailable"):
            integer(coverage[name])
    return row


def key(event):
    return identity(event["case"]), identity(event["model"]), integer(event["run_index"])


class Replay:
    """Projection only: terminal statuses remain owned by SuiteRunner."""
    def __init__(self, commit):
        self.commit, self.offset, self.total, self.sequence = commit, 0, None, None
        self.run_id, self.terminal, self.error = None, None, None
        self.queued, self.active, self.rows, self.records = {}, {}, {}, []

    def accept(self, event):
        if event.get("schema") != SCHEMA or self.terminal is not None:
            raise ValueError("unsupported or post-terminal event")
        kind = event["type"]
        if kind == "observation_error":
            self.error = "observation_error"
            return
        common = {"schema", "type", "run_id", "sequence"}
        fields = {"suite_started": {"total", "commit"}, "case_queued": {"case", "model", "run_index", "queue_position"},
                  "case_started": {"case", "model", "run_index"},
                  "case_progress": {"case", "model", "run_index", "phase", "elapsed_ms"},
                  "case_completed": {"row"}, "suite_completed": {"total", *STATUSES, "wall_time_ms"}}
        if kind not in fields or set(event) != common | fields[kind]:
            raise ValueError("unexpected progress payload")
        seq, run = integer(event["sequence"]), identity(event["run_id"])
        if self.sequence is not None and (seq != self.sequence + 1 or run != self.run_id):
            raise ValueError("event gap or mixed run")
        if self.total is None:
            if kind != "suite_started" or event["commit"] != self.commit or not 0 < integer(event["total"]) <= 16384:
                raise ValueError("missing or mismatched suite start")
        elif kind == "suite_started":
            raise ValueError("duplicate start")
        if kind == "suite_started":
            self.total = event["total"]
        elif kind == "case_queued":
            k = key(event)
            integer(event["queue_position"])
            if (k in self.queued or k in self.active or k in self.rows
                or len(self.queued)+len(self.active)+len(self.rows) >= self.total):
                raise ValueError("duplicate identity")
            self.queued[k] = dict(event)
        elif kind == "case_started":
            k = key(event)
            if k not in self.queued:
                raise ValueError("start without admission")
            self.queued.pop(k)
            self.active[k] = {**event, "elapsed_ms": 0}
        elif kind == "case_progress":
            k = key(event)
            integer(event["elapsed_ms"]), identity(event["phase"])
            if k not in self.active or event["elapsed_ms"] < self.active[k]["elapsed_ms"]:
                raise ValueError("invalid case progress")
            self.active[k] = dict(event)
        elif kind == "case_completed":
            row = safe_row(event["row"])
            k = key(row)
            if k in self.rows or len(self.rows) >= self.total:
                raise ValueError("duplicate or excess completion")
            if k not in self.active and row["status"] not in ("unavailable", "cancelled"):
                raise ValueError("executed result without start")
            if (k not in self.active and k not in self.queued
                and len(self.queued)+len(self.active)+len(self.rows) >= self.total):
                raise ValueError("completion outside planned inventory")
            elapsed = self.active.pop(k, {}).get("elapsed_ms")
            self.queued.pop(k, None)
            self.rows[k] = {**row, "case_elapsed_ms": elapsed}
        else:
            counts = Counter(row["status"] for row in self.rows.values())
            if (integer(event["total"]) != self.total or len(self.rows) != self.total
                or self.active or self.queued or any(integer(event[s]) != counts[s] for s in STATUSES)):
                raise ValueError("terminal counts disagree")
            integer(event["wall_time_ms"])
            self.terminal = event
        self.sequence, self.run_id = seq, run

    def read(self, path, final=False, emit=False):
        if self.error or not path.exists():
            return
        with path.open("rb") as stream:
            stream.seek(self.offset)
            while True:
                line = stream.readline(MAX_RECORD + 1)
                if not line:
                    break
                if len(line) > MAX_RECORD or self.offset + len(line) > MAX_FILE:
                    self.error = "progress_budget_exceeded"
                    break
                if not line.endswith(b"\n"):
                    if final:
                        self.error = "truncated_record"
                    break
                try:
                    event = json.loads(line, object_pairs_hook=unique_fields)
                    # Prepare publishable bytes before acceptance can change replay state.
                    record = json.dumps(event, ensure_ascii=False, allow_nan=False).encode()+b"\n"
                    self.accept(event)
                except (ValueError, KeyError, TypeError, AttributeError, RecursionError, OverflowError):
                    self.error = "invalid_progress"
                    break
                if self.error:
                    break
                self.records.append(record)
                self.offset += len(line)
                if emit:
                    self.print_event(event)

    def print_event(self, event):
        kind = event["type"]
        if kind == "case_completed":
            row = self.rows[key(event["row"])]
            counts = self.counts()
            elapsed = "not executed" if row["case_elapsed_ms"] is None else f"{row['case_elapsed_ms']/1000:.2f}s"
            label = json.dumps(f"{row['case']} / {row['model']} / run {row['run_index']+1}", ensure_ascii=False)
            print(f"[harness] {display_status(row).upper()} [{counts['completed']}/{self.total}] {label} · {elapsed}"
                  f" · exit={row['exit_code']} · error={row['failure_class'] or 'none'}"
                  f" · counts: {counts['passed']} passed, {counts['failed']} failed,"
                  f" {counts['cancelled']} cancelled, {counts['unavailable']} unavailable", flush=True)
            for line in row_checks(row):
                print("  " + json.dumps(line, ensure_ascii=False), flush=True)
        elif kind in ("suite_started", "case_started", "suite_completed") or (
            kind == "case_progress" and event["phase"] != "settled" and event["elapsed_ms"] % 30000 < 5000):
            print("[harness] " + json.dumps(event, ensure_ascii=False), flush=True)

    def counts(self):
        counts = Counter(row["status"] for row in self.rows.values())
        return {"planned":self.total, "completed":len(self.rows), **{s:counts[s] for s in STATUSES},
                "running":len(self.active), "waiting":len(self.queued),
                "unobserved":None if self.total is None else self.total-len(self.rows)-len(self.active)-len(self.queued)}

    def summary(self, execution, harness_outcome=None):
        complete = self.terminal is not None and self.error is None
        return {"commit":self.commit, "complete":complete,
                "success":complete and execution.get("exit_code") == 0 and execution.get("termination") == "finished"
                    and harness_outcome == "success"
                    and all(r["status"] == "passed" for r in self.rows.values()),
                "execution":execution, "harness_step":harness_outcome, "observation_error":self.error,
                "incomplete_reason":None if complete else self.error or
                    ("no_case_progress" if self.total is None else "missing_suite_terminal"),
                "counts":self.counts(), "runs":list(self.rows.values()),
                "running":list(self.active.values()), "waiting":list(self.queued.values()),
                "accounting":"Reported counters; missing coverage is unknown, not zero; not complete task-tree cost."}


def write_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def signal_process(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass  # Completion can race a deadline; the actual wait status remains authoritative.


def run(root, command, timeout, grace, started_at=None):
    start = time.monotonic()
    deadline = start + max(0, timeout - (max(0, time.time() - started_at) if started_at else 0))
    replay = Replay(os.environ["ASTRA_BUILD_SOURCE_GIT_SHA"])
    execution = {"exit_code":None, "termination":"running"}
    write_json(root / "execution.json", execution)
    requested_stop = False
    def request_stop(_signal, _frame):
        nonlocal requested_stop
        requested_stop = True
    handlers = {s:signal.signal(s, request_stop) for s in (signal.SIGINT, signal.SIGTERM)}
    try:
        if not os.environ.get("ASTRA_HARNESS_SUPERVISOR_IDENTITY"):
            print("[harness] Run through scripts/harness/process_supervisor.py; no cases started.", flush=True)
            execution = {"exit_code":2, "termination":"supervision_unavailable"}
            return 2
        help_result = subprocess.run([command[0], "--help"], stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, timeout=10)
        if help_result.returncode or b"--progress-file" not in help_result.stdout.split():
            print("[harness] Selected source does not support --progress-file; no cases started.", flush=True)
            execution = {"exit_code":2, "termination":"unsupported_source"}
            return 2
        with (root / "harness.log").open("wb") as log:
            process = subprocess.Popen([*command, "--progress-file", str(root / "progress.jsonl")],
                stdout=log, stderr=log, start_new_session=True)
            stopping = None
            try:
                while process.poll() is None:
                    before = (replay.offset, replay.error)
                    replay.read(root / "progress.jsonl", emit=True)
                    if before != (replay.offset, replay.error):
                        write_json(root / "summary.json", replay.summary(execution))
                    now = time.monotonic()
                    if stopping is None and (requested_stop or now >= deadline or replay.error):
                        execution["termination"] = "cancelled" if requested_stop else "deadline" if now >= deadline else "observation_error"
                        write_json(root / "execution.json", execution)
                        print(f"[harness] {execution['termination']}: requesting cancellation and preserving results.", flush=True)
                        signal_process(process, signal.SIGINT)
                        stopping = now
                    if stopping is not None and now - stopping >= grace:
                        signal_process(process, signal.SIGKILL)
                        break
                    time.sleep(0.2)
                code = process.wait(timeout=5)
            finally:
                if process.poll() is None:
                    signal_process(process, signal.SIGKILL)
                    process.wait(timeout=5)
            if requested_stop and stopping is None:
                execution["termination"] = "cancelled"
                stopping = time.monotonic()
            execution["exit_code"] = ({"deadline":124, "cancelled":130, "observation_error":1}[execution["termination"]]
                                      if stopping is not None else code)
            if stopping is None:
                execution["termination"] = "finished" if code >= 0 else "signal"
            replay.read(root / "progress.jsonl", final=True, emit=True)
            return execution["exit_code"] or (0 if replay.summary(execution, "success")["success"] else 1)
    finally:
        try:
            write_json(root / "execution.json", execution)
            write_json(root / "summary.json", replay.summary(execution))
        finally:
            for sig, handler in handlers.items():
                signal.signal(sig, handler)


def publish(root):
    replay = Replay(os.environ["ASTRA_BUILD_SOURCE_GIT_SHA"])
    replay.read(root / "progress.jsonl", final=True)
    execution = {"exit_code":None, "termination":"not_recorded"}
    if (root / "execution.json").exists():
        try:
            with (root / "execution.json").open("rb") as status:
                raw = status.read(4097)
            if len(raw) > 4096:
                raise ValueError("oversized execution status")
            execution = json.loads(raw, object_pairs_hook=unique_fields)
            if set(execution) != {"exit_code", "termination"} or execution["termination"] not in (
                "running", "finished", "signal", "deadline", "cancelled", "observation_error", "unsupported_source", "supervision_unavailable"):
                raise ValueError("invalid execution status")
            if execution["exit_code"] is not None and type(execution["exit_code"]) is not int:
                raise ValueError("invalid exit code")
        except (ValueError, TypeError, KeyError):
            execution = {"exit_code":None, "termination":"not_recorded"}
    outcome = os.environ.get("HARNESS_OUTCOME")
    if outcome not in ("success", "failure", "cancelled", "skipped"):
        outcome = None
    summary = replay.summary(execution, outcome)
    write_json(root / "summary.json", summary)
    (root / "progress.safe.jsonl").write_bytes(b"".join(replay.records))
    if not summary["success"]:
        print("[harness] Final failure analysis: " + json.dumps({
            "complete":summary["complete"], "termination":execution["termination"],
            "observation_error":summary["observation_error"], "counts":summary["counts"]}), flush=True)
        for row in summary["runs"]:
            if row["status"] != "passed":
                print("[harness] " + json.dumps({"case":row["case"], "model":row["model"],
                    "run_index":row["run_index"], "failure_class":row["failure_class"],
                    "analysis":row_checks(row)}, ensure_ascii=False), flush=True)
    def cell(value):
        return html.escape(str(value)).replace("|", "&#124;").replace("\n", " ")
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as out:
        out.write("## Live harness\n\n" + ("Complete — passed" if summary["success"] else
                  "Complete — failed" if summary["complete"] else "**Incomplete — not a pass**") +
                  "; termination: " + cell(execution["termination"]) + ".\n\n")
        if summary["incomplete_reason"]:
            out.write("Observation: " + cell(summary["incomplete_reason"]) + ".\n\n")
        out.write("Counts: `" + json.dumps(summary["counts"]) + "`\n\n")
        out.write("| Case / model / repetition | Result | Case elapsed | Error class |\n|---|---|---:|---|\n")
        for row in summary["runs"]:
            elapsed = "—" if row["case_elapsed_ms"] is None else f"{row['case_elapsed_ms']/1000:.2f}s"
            out.write(f"| {cell(row['case'])} / {cell(row['model'])} / {row['run_index']+1} | {display_status(row)} | {elapsed} | {cell(row['failure_class'] or '—')} |\n")
        for group in ("running", "waiting"):
            for item in summary[group]:
                out.write(f"| {cell(item['case'])} / {cell(item['model'])} / {item['run_index']+1} | {group} (not completed) | {item.get('elapsed_ms',0)/1000:.2f}s last observed | — |\n")
        out.write("\n### Check evidence and LLM judgments\n\n"
                  "Observed facts, not inferred root causes. Predicate numbers follow YAML order; raw prompts, tool results and judge/provider payloads remain private.\n\n")
        for row in summary["runs"]:
            out.write(f"#### {cell(row['case'])} / {cell(row['model'])} / {row['run_index']+1}\n\n")
            for line in row_checks(row):
                out.write(f"- {cell(line)}\n")
            out.write("\n")
        out.write("See summary.json for these diagnostics, step results, token/cache counters and unknown coverage.\n")
    return 0 if summary["success"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("run", "summary"))
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=float, default=3300)
    parser.add_argument("--grace-seconds", type=float, default=120)
    parser.add_argument("--started-at", type=float)
    argv = sys.argv[1:]
    boundary = argv.index("--") if "--" in argv else len(argv)
    args = parser.parse_args(argv[:boundary])
    if not all(math.isfinite(v) and v > 0 for v in (args.timeout_seconds, args.grace_seconds)):
        parser.error("deadlines must be positive")
    if args.mode == "summary":
        return publish(args.root)
    command = argv[boundary + 1:]
    if not command:
        parser.error("run requires a command")
    return run(args.root, command, args.timeout_seconds, args.grace_seconds, args.started_at)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        raise SystemExit("Harness observation failed; retained progress is incomplete, not a pass") from None
