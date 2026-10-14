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

SCHEMA = "astra.harness_progress.v1"
MAX_RECORD = 262_144
MAX_FILE = 32 * 1024 * 1024
STATUSES = ("passed", "failed", "cancelled", "unavailable")
METRICS = ("duration_ms", "ttft_ms", "turn_rounds", "tool_calls_count", "prompt_tokens",
           "cached_input_tokens", "cache_creation_tokens", "completion_tokens")


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


def safe_criteria(items, *, owner_passed):
    if not isinstance(items, list):
        raise ValueError("invalid criteria")
    for item in items:
        if set(item) != {"type", "severity", "score", "passed", "evaluation"}:
            raise ValueError("unexpected criterion payload")
        identity(item["type"])
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


def safe_row(row):
    if set(row) != {"case", "model", "run_index", "status", "has_warnings", "failure_class",
                    "exit_code", "criteria", "steps", "token_usage_coverage", "terminal_reported"}:
        raise ValueError("unexpected row payload")
    identity(row["case"]), identity(row["model"]), integer(row["run_index"])
    if row["status"] not in STATUSES or type(row["has_warnings"]) is not bool or type(row["exit_code"]) is not int:
        raise ValueError("invalid terminal row")
    if row["failure_class"] is not None:
        identity(row["failure_class"])
    safe_criteria(row["criteria"], owner_passed=row["status"] == "passed")
    if not isinstance(row["steps"], list) or set(row["terminal_reported"]) != set(METRICS):
        raise ValueError("invalid steps or metrics")
    for value in row["terminal_reported"].values():
        integer(value)
    for step in row["steps"]:
        if set(step) != {"step_index", "passed", "duration_ms", "criteria"} or type(step["passed"]) is not bool:
            raise ValueError("unexpected step payload")
        integer(step["step_index"]), integer(step["duration_ms"])
        safe_criteria(step["criteria"], owner_passed=step["passed"])
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
            failed = [f"root:{i}:{c['type']}" for i, c in enumerate(row["criteria"]) if c["passed"] is False]
            failed += [f"step:{s['step_index']}:{i}:{c['type']}" for s in row["steps"]
                       for i, c in enumerate(s["criteria"]) if c["passed"] is False]
            counts = self.counts()
            elapsed = "not executed" if row["case_elapsed_ms"] is None else f"{row['case_elapsed_ms']/1000:.2f}s"
            label = json.dumps(f"{row['case']} / {row['model']} / run {row['run_index']+1}", ensure_ascii=False)
            print(f"[harness] {row['status'].upper()} [{counts['completed']}/{self.total}] {label} · {elapsed}"
                  f" · exit={row['exit_code']} · error={row['failure_class'] or 'none'}"
                  f" · counts: {counts['passed']} passed, {counts['failed']} failed,"
                  f" {counts['cancelled']} cancelled, {counts['unavailable']} unavailable", flush=True)
            if failed:
                print("  Failed checks: " + json.dumps(failed), flush=True)
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
        for sig, handler in handlers.items():
            signal.signal(sig, handler)
        write_json(root / "execution.json", execution)
        write_json(root / "summary.json", replay.summary(execution))


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
    def cell(value):
        return html.escape(str(value)).replace("|", "&#124;").replace("\n", " ")
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as out:
        out.write("## GLM-5.3 harness\n\n" + ("Complete — passed" if summary["success"] else
                  "Complete — failed" if summary["complete"] else "**Incomplete — not a pass**") +
                  "; termination: " + cell(execution["termination"]) + ".\n\n")
        if summary["incomplete_reason"]:
            out.write("Observation: " + cell(summary["incomplete_reason"]) + ".\n\n")
        out.write("Counts: `" + json.dumps(summary["counts"]) + "`\n\n")
        out.write("| Case / model / repetition | Result | Case elapsed | Error class |\n|---|---|---:|---|\n")
        for row in summary["runs"]:
            elapsed = "—" if row["case_elapsed_ms"] is None else f"{row['case_elapsed_ms']/1000:.2f}s"
            out.write(f"| {cell(row['case'])} / {cell(row['model'])} / {row['run_index']+1} | {row['status']} | {elapsed} | {cell(row['failure_class'] or '—')} |\n")
        for group in ("running", "waiting"):
            for item in summary[group]:
                out.write(f"| {cell(item['case'])} / {cell(item['model'])} / {item['run_index']+1} | {group} (not completed) | {item.get('elapsed_ms',0)/1000:.2f}s last observed | — |\n")
        out.write("\nSee summary.json for failed checks, step results, token/cache counters and unknown coverage.\n")
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
