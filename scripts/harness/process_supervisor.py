#!/usr/bin/env python3
"""Supervise and reap a complete harness-owned process tree on Linux."""

from __future__ import annotations

import argparse
import ctypes
import os
import signal
import subprocess
import sys
import time
from pathlib import Path


PR_SET_PDEATHSIG = 1
PR_SET_CHILD_SUBREAPER = 36
LIBC = ctypes.CDLL(None, use_errno=True)


class SupervisorError(RuntimeError):
    pass


def _prctl(option: int, value: int) -> None:
    if LIBC.prctl(option, value, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def _starttime(pid: int) -> str:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except OSError as error:
        raise SupervisorError(f"owner PID {pid} is unavailable") from error
    tail = raw.rsplit(")", 1)
    fields = tail[1].strip().split() if len(tail) == 2 else []
    if len(fields) < 20:
        raise SupervisorError(f"owner PID {pid} has malformed proc state")
    return fields[19]


def _direct_children(pid: int) -> set[int]:
    try:
        raw = Path(f"/proc/{pid}/task/{pid}/children").read_text(encoding="ascii")
    except OSError:
        return set()
    return {int(value) for value in raw.split() if value.isdigit()}


def _descendants(pid: int) -> set[int]:
    result: set[int] = set()
    pending = list(_direct_children(pid))
    while pending:
        child = pending.pop()
        if child in result:
            continue
        result.add(child)
        pending.extend(_direct_children(child) - result)
    return result


def _signal_processes(pids: set[int], sig: int) -> None:
    for pid in sorted(pids, reverse=True):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError as error:
            raise SupervisorError(
                f"cannot signal owned descendant PID {pid}: {error}"
            ) from error


def _process_state(pid: int) -> str | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except OSError:
        return None
    tail = raw.rsplit(")", 1)
    fields = tail[1].strip().split() if len(tail) == 2 else []
    return fields[0] if fields else None


def _reap_adopted(primary_pid: int) -> None:
    for pid in _direct_children(os.getpid()) - {primary_pid}:
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            pass


def _live_descendants() -> set[int]:
    return {
        pid
        for pid in _descendants(os.getpid())
        if _process_state(pid) not in {None, "Z"}
    }


def _terminate_all(primary_pid: int, grace_seconds: float = 2.0) -> None:
    try:
        os.killpg(primary_pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + grace_seconds
    while time.monotonic() < deadline:
        _signal_processes(_descendants(os.getpid()), signal.SIGTERM)
        _reap_adopted(primary_pid)
        if not _live_descendants():
            return
        time.sleep(0.02)
    try:
        os.killpg(primary_pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    kill_deadline = time.monotonic() + 3.0
    while time.monotonic() < kill_deadline:
        descendants = _descendants(os.getpid())
        _signal_processes(descendants, signal.SIGKILL)
        _reap_adopted(primary_pid)
        if not _live_descendants():
            return
        time.sleep(0.02)
    remaining = sorted(_descendants(os.getpid()))
    raise SupervisorError(f"owned descendants survived SIGKILL: {remaining}")


def _child_preexec(expected_parent: int) -> None:
    _prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
    if os.getppid() != expected_parent:
        os._exit(78)


def supervise(owner_pid: int, identity: str, command: list[str]) -> int:
    if owner_pid <= 1 or not identity.strip() or not command:
        raise SupervisorError("owner PID, identity, and command are required")
    owner_starttime = _starttime(owner_pid)
    _prctl(PR_SET_CHILD_SUBREAPER, 1)
    supervisor_pid = os.getpid()
    received_signal = 0

    def request_stop(sig: int, _frame: object) -> None:
        nonlocal received_signal
        received_signal = received_signal or sig

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    environment = dict(os.environ)
    environment["ASTRA_HARNESS_SUPERVISOR_IDENTITY"] = identity
    child = subprocess.Popen(
        command,
        env=environment,
        start_new_session=True,
        preexec_fn=lambda: _child_preexec(supervisor_pid),
    )
    child_status: int | None = None
    try:
        while True:
            if received_signal:
                break
            try:
                if _starttime(owner_pid) != owner_starttime:
                    received_signal = signal.SIGTERM
                    break
            except SupervisorError:
                received_signal = signal.SIGTERM
                break
            polled = child.poll()
            if polled is not None:
                child_status = polled
                break
            _reap_adopted(child.pid)
            time.sleep(0.05)
    finally:
        _terminate_all(child.pid)
        try:
            polled = child.wait(timeout=0.1)
            if child_status is None:
                child_status = polled
        except subprocess.TimeoutExpired:
            pass
        _reap_adopted(child.pid)
    if _live_descendants():
        raise SupervisorError("descendant cleanup proof is not quiescent")
    if received_signal:
        return 128 + received_signal
    if child_status is None:
        return 78
    return 128 - child_status if child_status < 0 else child_status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)
    run = subparsers.add_parser("run")
    run.add_argument("--owner-pid", required=True, type=int)
    run.add_argument("--identity", required=True)
    run.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    try:
        return supervise(args.owner_pid, args.identity, command)
    except (OSError, SupervisorError) as error:
        print(f"astra harness: process supervision failed: {error}", file=sys.stderr)
        return 78


if __name__ == "__main__":
    raise SystemExit(main())
