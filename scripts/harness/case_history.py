#!/usr/bin/env python3
"""Reject accidental Terminal-Bench task reuse before a scored run."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import shutil
import sys
from pathlib import Path
from contextlib import contextmanager


class HistoryError(RuntimeError):
    pass


def _task_name(path: str) -> str:
    task_path = Path(path)
    value = task_path.name.strip()
    # Harbor's package cache stores a task below its content-addressed version:
    # ``.../<task-name>/<sha256>/task.toml``.  The version directory is an
    # implementation detail, not the stable benchmark task identity used by
    # selection and regression allowlists.  Preserve the final component for
    # ordinary dataset paths, where it already is the task name.
    if (
        re.fullmatch(r"[0-9a-f]{64}", value) is not None
        and (task_path / "task.toml").is_file()
    ):
        value = task_path.parent.name.strip()
    if not value or value in {".", ".."}:
        raise HistoryError(f"invalid task path {path!r}")
    return value


def _stable_task_name(value: object) -> str | None:
    if not isinstance(value, str) or not value:
        return None
    name = value.rstrip("/").rsplit("/", 1)[-1]
    return name if name and name not in {".", ".."} else None


def _completed_trial_tasks(job: Path, configured_paths: set[str]) -> set[str]:
    """Return configured tasks that have an official verifier reward.

    A generated Harbor config is only an intent to run a task.  Preflight can
    reject it, an operator can stop the job, or it can still be pending.  The
    reward ledger is Harbor's durable evidence that the official verifier
    completed, so it is the only history that should require an explicit
    regression marker on a later scored run.
    """
    completed: set[str] = set()
    for trial_result_path in job.glob("*/result.json"):
        try:
            trial = json.loads(trial_result_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            continue
        if not isinstance(trial, dict) or not trial.get("finished_at"):
            continue
        task_id = trial.get("task_id")
        task_path = task_id.get("path") if isinstance(task_id, dict) else None
        if not isinstance(task_path, str) or task_path not in configured_paths:
            continue
        verifier = trial.get("verifier_result")
        rewards = verifier.get("rewards") if isinstance(verifier, dict) else None
        if not isinstance(rewards, dict) or "reward" not in rewards:
            continue
        task = _stable_task_name(trial.get("task_name"))
        if task is not None:
            completed.add(task)
    return completed


def _recorded_tasks(jobs_dir: Path) -> set[str]:
    recorded: set[str] = set()
    if not jobs_dir.is_dir():
        return recorded
    for job in jobs_dir.iterdir():
        if not job.is_dir():
            continue
        config = job / "config.json"
        try:
            payload = json.loads(config.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            continue
        configured_paths: set[str] = set()
        for task in payload.get("tasks", []):
            if isinstance(task, dict) and isinstance(task.get("path"), str):
                configured_paths.add(task["path"])
        recorded.update(_completed_trial_tasks(job, configured_paths))
    return recorded


def _pid_start_time(pid: int) -> str | None:
    try:
        fields = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8").split()
    except OSError:
        return None
    return fields[21] if len(fields) > 21 else None


def _reservation_path(root: Path, task: str) -> Path:
    return root / hashlib.sha256(task.encode("utf-8")).hexdigest()


def _reservation_is_live(path: Path) -> bool:
    try:
        payload = json.loads((path / "lease.json").read_text(encoding="utf-8"))
        pid = payload["pid"]
        start_time = payload["pid_start_time"]
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, KeyError, TypeError):
        # Publication is atomic while the allocator lock is held. A malformed
        # entry therefore belongs to a crashed owner and can be reclaimed.
        return False
    return isinstance(pid, int) and _pid_start_time(pid) == start_time


@contextmanager
def _allocator_lock(root: Path):
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor = os.open(root / ".allocator.lock", os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(descriptor, fcntl.LOCK_UN)
        os.close(descriptor)


def _write_json_atomic(path: Path, payload: object) -> None:
    encoded = json.dumps(payload, sort_keys=True).encode("utf-8")
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    descriptor = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def _reserve_selection_locked(
    tasks: list[str], root: Path, owner_pid: int, manifest: Path | None = None
) -> None:
    start_time = _pid_start_time(owner_pid)
    if start_time is None:
        raise HistoryError(f"reservation owner pid {owner_pid} is not live")
    acquired: list[Path] = []
    normalized_tasks = sorted({_task_name(task) for task in tasks})
    try:
        for task in normalized_tasks:
            path = _reservation_path(root, task)
            if path.exists():
                if _reservation_is_live(path):
                    raise HistoryError(f"task is already reserved by a live scored run: {task}")
                shutil.rmtree(path)
            path.mkdir(mode=0o700)
            acquired.append(path)
            lease = {"schema": "astra.harness.case-reservation.v1", "task": task,
                     "pid": owner_pid, "pid_start_time": start_time}
            _write_json_atomic(path / "lease.json", lease)
        if manifest is not None:
            _write_json_atomic(
                manifest,
                {"schema": "astra.harness.case-reservation-manifest.v1",
                 "root": str(root), "pid": owner_pid,
                 "pid_start_time": start_time, "tasks": normalized_tasks},
            )
    except Exception:
        for path in acquired:
            shutil.rmtree(path, ignore_errors=True)
        raise


def reserve_selection(
    tasks: list[str], root: Path, owner_pid: int, manifest: Path | None = None
) -> None:
    with _allocator_lock(root):
        _reserve_selection_locked(tasks, root, owner_pid, manifest)


def release_selection(tasks: list[str], root: Path, owner_pid: int) -> None:
    start_time = _pid_start_time(owner_pid)
    with _allocator_lock(root):
        for task in {_task_name(task) for task in tasks}:
            path = _reservation_path(root, task)
            try:
                payload = json.loads((path / "lease.json").read_text(encoding="utf-8"))
            except (OSError, UnicodeDecodeError, json.JSONDecodeError):
                continue
            if payload.get("pid") == owner_pid and payload.get("pid_start_time") == start_time:
                shutil.rmtree(path)


def release_reservation_manifest(manifest: Path) -> None:
    try:
        payload = json.loads(manifest.read_text(encoding="utf-8"))
        root = Path(payload["root"])
        owner_pid = payload["pid"]
        tasks = payload["tasks"]
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, KeyError, TypeError):
        raise HistoryError("reservation manifest is malformed")
    if not isinstance(owner_pid, int) or not isinstance(tasks, list) or not all(
        isinstance(task, str) for task in tasks
    ):
        raise HistoryError("reservation manifest has invalid ownership fields")
    release_selection(tasks, root, owner_pid)
    manifest.unlink(missing_ok=True)


def validate_selection(
    tasks: list[str], recorded: set[str], allowed_regressions: set[str]
) -> dict[str, object]:
    requested = [_task_name(task) for task in tasks]
    if len(requested) < 3 or len(set(requested)) != len(requested):
        raise HistoryError("a scored round must contain at least three unique tasks")
    if not allowed_regressions.issubset(set(requested)):
        raise HistoryError("regression allowlist contains a task outside this round")
    repeated = set(requested) & recorded
    unauthorized = repeated - allowed_regressions
    if unauthorized:
        raise HistoryError(
            "previously measured task(s) require an explicit regression allowlist: "
            + ", ".join(sorted(unauthorized))
        )
    return {
        "schema": "astra.harness.case-history.v1",
        "requested_tasks": requested,
        "new_tasks": sorted(set(requested) - recorded),
        "regression_tasks": sorted(repeated),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs-dir", type=Path, required=True)
    parser.add_argument("--task", action="append", default=[])
    parser.add_argument("--allow-regression-task", action="append", default=[])
    parser.add_argument("--reservation-dir", type=Path)
    parser.add_argument("--reservation-owner-pid", type=int)
    parser.add_argument("--reservation-manifest", type=Path)
    parser.add_argument("--release-reservation", action="store_true")
    args = parser.parse_args()
    try:
        if (args.reservation_dir is None) != (args.reservation_owner_pid is None):
            raise HistoryError("reservation directory and owner pid must be provided together")
        if args.release_reservation:
            if args.reservation_manifest is not None:
                release_reservation_manifest(args.reservation_manifest)
                print(json.dumps({"schema": "astra.harness.case-history.v1", "released": True}))
                return 0
            if args.reservation_dir is None or args.reservation_owner_pid is None:
                raise HistoryError("release requires a reservation directory and owner pid")
            release_selection(args.task, args.reservation_dir, args.reservation_owner_pid)
            print(json.dumps({"schema": "astra.harness.case-history.v1", "released": True}))
            return 0
        if args.reservation_dir is not None and args.reservation_owner_pid is not None:
            # A completed official verifier result and the previous owner's
            # release must not leave a handoff window for a second launcher.
            # Read history, validate, and publish the new lease under the
            # same allocator lock used by release.
            with _allocator_lock(args.reservation_dir):
                result = validate_selection(
                    args.task,
                    _recorded_tasks(args.jobs_dir),
                    {_task_name(task) for task in args.allow_regression_task},
                )
                _reserve_selection_locked(
                    args.task, args.reservation_dir, args.reservation_owner_pid,
                    args.reservation_manifest,
                )
        else:
            result = validate_selection(
                args.task,
                _recorded_tasks(args.jobs_dir),
                {_task_name(task) for task in args.allow_regression_task},
            )
        if args.reservation_dir is not None:
            result["reservation"] = "acquired"
    except HistoryError as error:
        print(f"astra harness: case history validation failed: {error}", file=sys.stderr)
        return 78
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
