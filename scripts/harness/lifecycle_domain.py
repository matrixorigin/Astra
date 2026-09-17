#!/usr/bin/env python3
"""Own one benchmark's leases, descendants, snapshots, and Docker projects."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import secrets
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import lifecycle_broker

try:
    from recovery_environment import project_recovery_compose_env
except ModuleNotFoundError:
    from scripts.harness.recovery_environment import project_recovery_compose_env


SCHEMA = "astra.harness.lifecycle_domain.v1"
DOCKER_RECORD_SCHEMA = "astra.harness.docker_project.v1"
PR_SET_PDEATHSIG = 1
PR_SET_CHILD_SUBREAPER = 36
LIBC = ctypes.CDLL(None, use_errno=True)


class DomainError(RuntimeError):
    pass


def _prctl(option: int, value: int) -> None:
    if LIBC.prctl(option, value, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise DomainError(f"prctl({option}) failed: {os.strerror(error)}")


def process_starttime(pid: int) -> str:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except OSError as error:
        raise DomainError(f"process {pid} is unavailable") from error
    fields = raw.rsplit(")", 1)[-1].strip().split()
    if len(fields) < 20:
        raise DomainError(f"process {pid} has malformed proc state")
    return fields[19]


def _same_process(pid: int, starttime: str) -> bool:
    try:
        return process_starttime(pid) == starttime
    except DomainError:
        return False


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


def _state(pid: int) -> str | None:
    try:
        fields = (
            Path(f"/proc/{pid}/stat")
            .read_text(encoding="ascii")
            .rsplit(")", 1)[-1]
            .strip()
            .split()
        )
        return fields[0] if fields else None
    except OSError:
        return None


def _reap() -> None:
    while True:
        try:
            waited, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if waited <= 0:
            return


def _signal(pids: set[int], sig: int) -> None:
    for pid in sorted(pids, reverse=True):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError as error:
            raise DomainError(
                f"cannot signal owned descendant {pid}: {error}"
            ) from error


def terminate_domain(exclude: set[int] | None = None) -> None:
    exclude = exclude or set()
    deadline = time.monotonic() + 3.0
    while time.monotonic() < deadline:
        targets = _descendants(os.getpid()) - exclude
        _signal(targets, signal.SIGTERM)
        _reap()
        if not {pid for pid in targets if _state(pid) not in {None, "Z"}}:
            break
        time.sleep(0.02)
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        targets = _descendants(os.getpid()) - exclude
        _signal(targets, signal.SIGKILL)
        _reap()
        if not {pid for pid in targets if _state(pid) not in {None, "Z"}}:
            return
        time.sleep(0.02)
    remaining = sorted(
        pid for pid in _descendants(os.getpid()) - exclude if _state(pid) != "Z"
    )
    if remaining:
        raise DomainError(f"owned process domain did not become quiescent: {remaining}")


def _read_closed_json(path: Path, expected: set[str]) -> dict:
    if path.is_symlink() or not path.is_file():
        raise DomainError(f"lifecycle ledger entry is not a regular file: {path}")
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or set(value) != expected:
        raise DomainError(f"lifecycle ledger entry is not canonical: {path}")
    return value


def _docker_records(state_dir: Path) -> list[Path]:
    directory = state_dir / "docker-projects"
    if not directory.exists():
        return []
    if directory.is_symlink() or not directory.is_dir():
        raise DomainError("Docker project ledger is not a real directory")
    return sorted(directory.glob("*.json"))


def cleanup_docker_projects(state_dir: Path) -> None:
    for record_path in _docker_records(state_dir):
        record = _read_closed_json(
            record_path,
            {
                "schema",
                "project",
                "project_directory",
                "compose_files",
                "compose_env",
            },
        )
        project = record["project"]
        project_directory = record["project_directory"]
        compose_files = record["compose_files"]
        compose_env = record["compose_env"]
        try:
            compose_env = project_recovery_compose_env(compose_env)
        except ValueError as error:
            raise DomainError(
                f"Docker cleanup record is invalid: {record_path}: {error}"
            ) from error
        if (
            record["schema"] != DOCKER_RECORD_SCHEMA
            or not isinstance(project, str)
            or not project
            or not isinstance(project_directory, str)
            or not isinstance(compose_files, list)
            or not compose_files
            or not all(isinstance(path, str) and path for path in compose_files)
        ):
            raise DomainError(f"Docker cleanup record is invalid: {record_path}")
        command = [
            "docker",
            "compose",
            "--project-name",
            project,
            "--project-directory",
            project_directory,
        ]
        for path in compose_files:
            command.extend(["-f", path])
        command.extend(["down", "--volumes", "--remove-orphans"])
        try:
            completed = subprocess.run(
                command,
                check=False,
                capture_output=True,
                text=True,
                timeout=60,
                env={
                    "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                    **compose_env,
                },
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise DomainError(
                f"cannot clean Docker project {project}: {error}"
            ) from error
        if completed.returncode != 0:
            detail = (completed.stdout or completed.stderr or "compose down failed")[
                -1000:
            ]
            raise DomainError(f"cannot clean Docker project {project}: {detail}")
        record_path.unlink()


def _make_writable(root: Path) -> None:
    if not root.exists():
        return
    for path in [root, *root.rglob("*")]:
        if path.is_symlink():
            continue
        try:
            path.chmod(0o700 if path.is_dir() else 0o600)
        except OSError:
            pass


def _remove_state(state_dir: Path) -> None:
    _make_writable(state_dir)
    shutil.rmtree(state_dir)


def _owner_record(state_dir: Path) -> dict:
    return _read_closed_json(
        state_dir / "owner.json",
        {
            "schema",
            "database_identity",
            "gateway_port",
            "witness_pid",
            "witness_starttime",
        },
    )


def recover_stale_domains(parent: Path) -> None:
    parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if parent.is_symlink() or not parent.is_dir():
        raise DomainError("lifecycle state parent must be a real directory")
    parent.chmod(0o700)
    for state_dir in sorted(path for path in parent.iterdir() if path.is_dir()):
        record = _owner_record(state_dir)
        if record["schema"] != SCHEMA:
            raise DomainError(f"unknown lifecycle state schema: {state_dir}")
        pid = record["witness_pid"]
        starttime = record["witness_starttime"]
        identity = record["database_identity"]
        port = record["gateway_port"]
        if (
            not isinstance(pid, int)
            or not isinstance(starttime, str)
            or not isinstance(identity, str)
            or not isinstance(port, int)
        ):
            raise DomainError(f"invalid lifecycle owner record: {state_dir}")
        if _same_process(pid, starttime):
            continue
        # Acquiring both generations is the recovery authority.  If either is
        # still live, cleanup must not touch that run's Docker or snapshot state.
        try:
            witness = lifecycle_broker.LifecycleLease.acquire(identity, port, "witness")
            try:
                primary = lifecycle_broker.LifecycleLease.acquire(
                    identity, port, "primary"
                )
                try:
                    runtime = lifecycle_broker.LifecycleLease.acquire(
                        identity, port, "runtime"
                    )
                except BaseException:
                    primary.close()
                    raise
            except BaseException:
                witness.close()
                raise
        except lifecycle_broker.LifecycleLeaseBusy:
            continue
        try:
            cleanup_docker_projects(state_dir)
            _remove_state(state_dir)
        finally:
            runtime.close()
            primary.close()
            witness.close()


def _write_owner(state_dir: Path, identity: str, port: int) -> None:
    path = state_dir / "owner.json"
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o400,
    )
    payload = {
        "schema": SCHEMA,
        "database_identity": identity,
        "gateway_port": port,
        "witness_pid": os.getpid(),
        "witness_starttime": process_starttime(os.getpid()),
    }
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(payload, stream, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def _child_preexec(expected_parent: int) -> None:
    _prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
    if os.getppid() != expected_parent:
        os._exit(78)


def _guardian(
    witness_pid: int,
    witness_starttime: str,
    identity: str,
    port: int,
    state_dir: Path,
    command: list[str],
) -> int:
    _prctl(PR_SET_CHILD_SUBREAPER, 1)
    lease = lifecycle_broker.LifecycleLease.acquire(identity, port, "primary")
    try:
        runtime_lease = lifecycle_broker.LifecycleLease.acquire(
            identity, port, "runtime"
        )
    except BaseException:
        lease.close()
        raise
    stopping = 0

    def stop(sig: int, _frame: object) -> None:
        nonlocal stopping
        stopping = stopping or sig

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    environment = dict(os.environ)
    environment.update(
        {
            "ASTRA_HARNESS_DOMAIN_ACTIVE": "1",
            "ASTRA_HARNESS_DOMAIN_STATE": str(state_dir),
            "ASTRA_HARNESS_LIFECYCLE_GUARDIAN_PID": str(os.getpid()),
            "ASTRA_HARNESS_LIFECYCLE_WITNESS_PID": str(witness_pid),
        }
    )
    guardian_pid = os.getpid()
    for descriptor in runtime_lease.descriptors:
        os.set_inheritable(descriptor, True)
    try:
        runner = subprocess.Popen(
            command,
            env=environment,
            preexec_fn=lambda: _child_preexec(guardian_pid),
            pass_fds=runtime_lease.descriptors,
        )
    finally:
        for descriptor in runtime_lease.descriptors:
            os.set_inheritable(descriptor, False)
    status: int | None = None
    try:
        while True:
            if stopping:
                break
            if not _same_process(witness_pid, witness_starttime):
                stopping = signal.SIGTERM
                break
            adopted = {
                pid
                for pid in _direct_children(os.getpid()) - {runner.pid}
                if _state(pid) not in {None, "Z"}
            }
            if adopted:
                # An inner supervisor died while its detached descendants lived.
                stopping = signal.SIGTERM
                break
            polled = runner.poll()
            if polled is not None:
                status = polled
                break
            time.sleep(0.04)
    finally:
        terminate_domain()
        try:
            polled = runner.wait(timeout=0.2)
            status = polled if status is None else status
        except subprocess.TimeoutExpired:
            pass
        cleanup_docker_projects(state_dir)
        runtime_lease.close()
        lease.close()
    if stopping:
        return 128 + stopping
    return 78 if status is None else (128 - status if status < 0 else status)


def run(identity: str, port: int, state_parent: Path, command: list[str]) -> int:
    if not command:
        raise DomainError("lifecycle domain command is required")
    _prctl(PR_SET_CHILD_SUBREAPER, 1)
    recover_stale_domains(state_parent)
    state_dir = state_parent / (
        f"domain-{identity[:12]}-{os.getpid()}-{secrets.token_hex(6)}"
    )
    os.mkdir(state_dir, 0o700)
    os.mkdir(state_dir / "docker-projects", 0o700)
    _write_owner(state_dir, identity, port)
    witness_lease = lifecycle_broker.LifecycleLease.acquire(identity, port, "witness")
    witness_pid = os.getpid()
    witness_starttime = process_starttime(witness_pid)
    stopping = 0

    def stop(sig: int, _frame: object) -> None:
        nonlocal stopping
        stopping = stopping or sig

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    guardian = os.fork()
    if guardian == 0:
        try:
            witness_lease.close()
            status = _guardian(
                witness_pid,
                witness_starttime,
                identity,
                port,
                state_dir,
                command,
            )
        except BaseException as error:
            print(f"astra harness: lifecycle guardian failed: {error}", file=sys.stderr)
            status = 78
        os._exit(status)
    status: int | None = None
    try:
        while True:
            if stopping:
                break
            try:
                waited, raw = os.waitpid(guardian, os.WNOHANG)
            except ChildProcessError:
                waited, raw = guardian, 78 << 8
            if waited == guardian:
                status = os.waitstatus_to_exitcode(raw)
                break
            time.sleep(0.04)
    finally:
        # If the guardian was SIGKILLed, every orphaned process is adopted here.
        terminate_domain()
        cleanup_docker_projects(state_dir)
        witness_lease.close()
        _remove_state(state_dir)
    if stopping:
        return 128 + stopping
    return 78 if status is None else status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database-identity", required=True)
    parser.add_argument("--gateway-port", type=int, required=True)
    parser.add_argument("--state-parent", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    try:
        return run(
            args.database_identity,
            args.gateway_port,
            args.state_parent.resolve(),
            command,
        )
    except (
        DomainError,
        lifecycle_broker.LifecycleLeaseError,
        OSError,
        ValueError,
        json.JSONDecodeError,
    ) as error:
        print(f"astra harness: lifecycle domain failed: {error}", file=sys.stderr)
        return 78


if __name__ == "__main__":
    raise SystemExit(main())
