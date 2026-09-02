#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import ast
import json
import os
import secrets
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_DIR = Path(__file__).resolve().parent
RUNNER_PATH = SCRIPT_DIR / "run_terminal_bench_current.sh"


def load_module(name: str):
    path = SCRIPT_DIR / f"{name}.py"
    if not path.is_file():
        raise AssertionError(f"missing harness contract module: {path}")
    spec = importlib.util.spec_from_file_location(f"astra_harness_{name}", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _read_available_stderr(process: subprocess.Popen) -> str:
    if process.stderr is None:
        return "<stderr unavailable>"
    descriptor = process.stderr.fileno()
    os.set_blocking(descriptor, False)
    chunks: list[bytes] = []
    while True:
        try:
            chunk = os.read(descriptor, 65536)
        except BlockingIOError:
            break
        if not chunk:
            break
        chunks.append(chunk)
    detail = b"".join(chunks).decode(errors="replace").strip()
    return detail or "<empty stderr>"


def wait_for(
    predicate,
    timeout: float = 5.0,
    *,
    process: subprocess.Popen | None = None,
    label: str = "condition",
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        if process is not None and process.poll() is not None:
            raise AssertionError(
                f"{label} failed because domain exited with status "
                f"{process.returncode}; stderr: {_read_available_stderr(process)}"
            )
        time.sleep(0.02)
    raise AssertionError(f"{label} did not become true before timeout")


class LifecycleAndSnapshotTests(unittest.TestCase):
    def _unique_lifecycle_resource(self) -> tuple[str, int]:
        reservation = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        reservation.bind(("127.0.0.1", 0))
        self.addCleanup(reservation.close)
        return secrets.token_hex(32), reservation.getsockname()[1]

    def test_wait_for_reports_early_domain_stderr(self):
        process = subprocess.Popen(
            [
                "python3",
                "-c",
                "import sys; print('unique lease collision', file=sys.stderr); raise SystemExit(78)",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        with self.assertRaisesRegex(AssertionError, "unique lease collision"):
            wait_for(lambda: False, process=process, label="domain readiness")
        process.wait(timeout=5)
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()

    def test_docker_recovery_environment_roundtrip_is_exact_and_secret_free(self):
        with mock.patch.object(sys, "path", [str(SCRIPT_DIR), *sys.path]):
            readiness = load_module("verifier_readiness")
            lifecycle = load_module("lifecycle_domain")

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "docker-projects").mkdir()
            compose = root / "compose.yaml"
            compose.write_text("services: {}\n", encoding="utf-8")

            class Environment:
                session_id = "recovery-roundtrip"
                environment_dir = root
                _docker_compose_paths = [compose]
                compose_env = {
                    "MAIN_IMAGE_NAME": "example/task@sha256:" + "1" * 64,
                    "CONTEXT_DIR": str(root),
                    "HOST_VERIFIER_LOGS_PATH": str(root / "logs"),
                }

                def _compose_env_vars(self, *, include_os_env):
                    self.assert_no_os_env = not include_os_env
                    return dict(self.compose_env)

            environment = Environment()
            readiness._write_cleanup_record(environment, root)
            records = list((root / "docker-projects").glob("*.json"))
            self.assertEqual(len(records), 1)
            persisted = json.loads(records[0].read_text(encoding="utf-8"))
            self.assertEqual(persisted["compose_env"], environment.compose_env)
            completed = subprocess.CompletedProcess([], 0, "", "")
            with mock.patch.object(
                lifecycle.subprocess, "run", return_value=completed
            ) as docker:
                lifecycle.cleanup_docker_projects(root)
            self.assertFalse(records[0].exists())
            invoked_env = docker.call_args.kwargs["env"]
            self.assertEqual(
                {key: value for key, value in invoked_env.items() if key != "PATH"},
                environment.compose_env,
            )

            for name, value in (
                ("DATABASE_URL", "postgres://user:sentinel@db/name"),
                ("FOO", "sentinel-arbitrary"),
            ):
                environment.compose_env = {
                    "MAIN_IMAGE_NAME": "example/task:latest",
                    name: value,
                }
                with (
                    self.subTest(name=name),
                    self.assertRaisesRegex(
                        readiness.ReadinessError, "non-recovery keys"
                    ),
                ):
                    readiness._write_cleanup_record(environment, root)

            tampered = root / "docker-projects" / "tampered.json"
            tampered.write_text(
                json.dumps(
                    {
                        **persisted,
                        "project": "tampered",
                        "compose_env": {"FOO": "sentinel-reloaded"},
                    }
                ),
                encoding="utf-8",
            )
            with (
                mock.patch.object(lifecycle.subprocess, "run") as docker,
                self.assertRaisesRegex(lifecycle.DomainError, "non-recovery keys"),
            ):
                lifecycle.cleanup_docker_projects(root)
            docker.assert_not_called()

    def _start_crash_domain(self, root: Path, identity: str, port: int):
        child = root / "domain_child.py"
        child.write_text(
            """\
import os
import time
from pathlib import Path

root = Path(os.environ["DOMAIN_TEST_ROOT"])
root.joinpath("guardian.pid").write_text(
    os.environ["ASTRA_HARNESS_LIFECYCLE_GUARDIAN_PID"]
)
if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        root.joinpath("detached.pid").write_text(str(os.getpid()))
        time.sleep(60)
    os._exit(0)
time.sleep(60)
"""
        )
        process = subprocess.Popen(
            [
                "python3",
                str(SCRIPT_DIR / "lifecycle_domain.py"),
                "--database-identity",
                identity,
                "--gateway-port",
                str(port),
                "--state-parent",
                str(root / "state"),
                "--",
                "python3",
                str(child),
            ],
            env={
                **os.environ,
                "PYTHONPATH": str(SCRIPT_DIR),
                "DOMAIN_TEST_ROOT": str(root),
            },
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            wait_for(
                lambda: (root / "guardian.pid").is_file(),
                process=process,
                label="guardian.pid",
            )
            wait_for(
                lambda: (root / "detached.pid").is_file(),
                process=process,
                label="detached.pid",
            )
        except BaseException:
            if process.poll() is None:
                process.kill()
                process.wait()
            if process.stdout is not None:
                process.stdout.close()
            if process.stderr is not None:
                process.stderr.close()
            raise
        return process

    def test_guardian_sigkill_reaps_detached_descendant_before_lease_release(self):
        broker = load_module("lifecycle_broker")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            identity, port = self._unique_lifecycle_resource()
            domain = self._start_crash_domain(root, identity, port)
            guardian = int((root / "guardian.pid").read_text())
            detached = int((root / "detached.pid").read_text())
            try:
                os.kill(guardian, signal.SIGKILL)
                self.assertNotEqual(domain.wait(timeout=10), 0)
                wait_for(lambda: not Path(f"/proc/{detached}").exists())
                with broker.LifecycleLease.acquire(identity, port, "witness"):
                    with broker.LifecycleLease.acquire(identity, port, "primary"):
                        pass
                self.assertEqual(list((root / "state").iterdir()), [])
            finally:
                if domain.poll() is None:
                    domain.kill()
                    domain.wait()
                if domain.stdout is not None:
                    domain.stdout.close()
                if domain.stderr is not None:
                    domain.stderr.close()

    def test_witness_sigkill_fences_competitor_until_guardian_quiescence(self):
        broker = load_module("lifecycle_broker")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            identity, port = self._unique_lifecycle_resource()
            domain = self._start_crash_domain(root, identity, port)
            detached = int((root / "detached.pid").read_text())
            guardian = int((root / "guardian.pid").read_text())
            domain.kill()
            domain.wait(timeout=5)
            # The witness name may become available first, but a competing
            # launch cannot own both generations while cleanup is incomplete.
            try:
                witness = broker.LifecycleLease.acquire(identity, port, "witness")
            except broker.LifecycleLeaseBusy:
                witness = None
            if witness is not None:
                try:
                    try:
                        primary = broker.LifecycleLease.acquire(
                            identity, port, "primary"
                        )
                    except broker.LifecycleLeaseBusy:
                        primary = None
                    if primary is not None:
                        try:
                            with self.assertRaises(broker.LifecycleLeaseBusy):
                                broker.LifecycleLease.acquire(identity, port, "runtime")
                        finally:
                            primary.close()
                finally:
                    witness.close()
            wait_for(lambda: not Path(f"/proc/{detached}").exists(), timeout=10)
            wait_for(lambda: not Path(f"/proc/{guardian}").exists(), timeout=10)
            recovery = subprocess.run(
                [
                    "python3",
                    str(SCRIPT_DIR / "lifecycle_domain.py"),
                    "--database-identity",
                    identity,
                    "--gateway-port",
                    str(port),
                    "--state-parent",
                    str(root / "state"),
                    "--",
                    "true",
                ],
                env={**os.environ, "PYTHONPATH": str(SCRIPT_DIR)},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(recovery.returncode, 0, recovery.stderr)
            self.assertEqual(list((root / "state").iterdir()), [])
            if domain.stdout is not None:
                domain.stdout.close()
            if domain.stderr is not None:
                domain.stderr.close()

    def test_inner_supervisor_sigkill_cancels_domain_and_reaps_double_fork(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            identity, port = self._unique_lifecycle_resource()
            worker = root / "worker.py"
            worker.write_text(
                """\
import os, time
from pathlib import Path
if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        Path(os.environ["DOMAIN_TEST_ROOT"], "detached.pid").write_text(str(os.getpid()))
        time.sleep(60)
    os._exit(0)
time.sleep(60)
"""
            )
            runner = root / "runner.py"
            runner.write_text(
                """\
import os, subprocess, time
from pathlib import Path
root = Path(os.environ["DOMAIN_TEST_ROOT"])
p = subprocess.Popen([
    "python3", os.environ["SUPERVISOR"], "run",
    "--owner-pid", str(os.getpid()), "--identity", "crash", "--",
    "python3", os.environ["WORKER"],
])
root.joinpath("supervisor.pid").write_text(str(p.pid))
p.wait()
time.sleep(60)
"""
            )
            environment = {
                **os.environ,
                "PYTHONPATH": str(SCRIPT_DIR),
                "DOMAIN_TEST_ROOT": str(root),
                "SUPERVISOR": str(SCRIPT_DIR / "process_supervisor.py"),
                "WORKER": str(worker),
            }
            domain = subprocess.Popen(
                [
                    "python3",
                    str(SCRIPT_DIR / "lifecycle_domain.py"),
                    "--database-identity",
                    identity,
                    "--gateway-port",
                    str(port),
                    "--state-parent",
                    str(root / "state"),
                    "--",
                    "python3",
                    str(runner),
                ],
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                wait_for(
                    lambda: (root / "supervisor.pid").is_file(),
                    process=domain,
                    label="supervisor.pid",
                )
                wait_for(
                    lambda: (root / "detached.pid").is_file(),
                    process=domain,
                    label="detached.pid",
                )
                supervisor = int((root / "supervisor.pid").read_text())
                detached = int((root / "detached.pid").read_text())
                os.kill(supervisor, signal.SIGKILL)
                self.assertNotEqual(domain.wait(timeout=10), 0)
                wait_for(lambda: not Path(f"/proc/{detached}").exists())
            finally:
                if domain.poll() is None:
                    domain.kill()
                    domain.wait()
                if domain.stdout is not None:
                    domain.stdout.close()
                if domain.stderr is not None:
                    domain.stderr.close()

    def test_double_custodian_crash_keeps_runtime_lease_until_last_descendant(self):
        broker = load_module("lifecycle_broker")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            identity, port = self._unique_lifecycle_resource()
            domain = self._start_crash_domain(root, identity, port)
            guardian = int((root / "guardian.pid").read_text())
            detached = int((root / "detached.pid").read_text())
            os.kill(guardian, signal.SIGSTOP)
            domain.kill()
            domain.wait(timeout=5)
            os.kill(guardian, signal.SIGKILL)
            wait_for(lambda: not Path(f"/proc/{guardian}").exists())
            self.assertTrue(Path(f"/proc/{detached}").exists())
            witness = broker.LifecycleLease.acquire(identity, port, "witness")
            primary = broker.LifecycleLease.acquire(identity, port, "primary")
            try:
                with self.assertRaises(broker.LifecycleLeaseBusy):
                    broker.LifecycleLease.acquire(identity, port, "runtime")
            finally:
                primary.close()
                witness.close()
            os.kill(detached, signal.SIGKILL)
            wait_for(lambda: not Path(f"/proc/{detached}").exists())
            recovery = subprocess.run(
                [
                    "python3",
                    str(SCRIPT_DIR / "lifecycle_domain.py"),
                    "--database-identity",
                    identity,
                    "--gateway-port",
                    str(port),
                    "--state-parent",
                    str(root / "state"),
                    "--",
                    "true",
                ],
                env={**os.environ, "PYTHONPATH": str(SCRIPT_DIR)},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(recovery.returncode, 0, recovery.stderr)
            self.assertEqual(list((root / "state").iterdir()), [])
            if domain.stdout is not None:
                domain.stdout.close()
            if domain.stderr is not None:
                domain.stderr.close()

    def test_owner_sigkill_releases_abstract_database_and_gateway_names(self):
        broker = load_module("lifecycle_broker")
        identity, port = self._unique_lifecycle_resource()
        owner = subprocess.Popen(["sleep", "30"])
        holder = subprocess.Popen(
            [
                "python3",
                str(SCRIPT_DIR / "lifecycle_broker.py"),
                "hold",
                "--database-identity",
                identity,
                "--gateway-port",
                str(port),
                "--owner-pid",
                str(owner.pid),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            wait_for(lambda: broker.lifecycle_names_are_bound(identity, port))
            owner.send_signal(signal.SIGKILL)
            owner.wait(timeout=5)
            self.assertEqual(holder.wait(timeout=5), 0, holder.stderr.read())
            with broker.LifecycleLease.acquire(identity, port):
                pass
        finally:
            if owner.poll() is None:
                owner.kill()
                owner.wait()
            if holder.poll() is None:
                holder.terminate()
                holder.wait()
            if holder.stdout is not None:
                holder.stdout.close()
            if holder.stderr is not None:
                holder.stderr.close()

    def test_supervisor_reaps_setsid_double_fork_descendant(self):
        load_module("process_supervisor")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            grandchild_pid = root / "grandchild.pid"
            child = root / "double_fork.py"
            child.write_text(
                """\
import os
import pathlib
import time

if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        pathlib.Path(os.environ[\"GRANDCHILD_PID\"]).write_text(str(os.getpid()))
        time.sleep(30)
    os._exit(0)
time.sleep(30)
"""
            )
            supervisor = subprocess.Popen(
                [
                    "python3",
                    str(SCRIPT_DIR / "process_supervisor.py"),
                    "run",
                    "--owner-pid",
                    str(os.getpid()),
                    "--identity",
                    "double-fork-regression",
                    "--",
                    "python3",
                    str(child),
                ],
                env={**os.environ, "GRANDCHILD_PID": str(grandchild_pid)},
            )
            try:
                wait_for(grandchild_pid.is_file)
                descendant = int(grandchild_pid.read_text())
                self.assertTrue(Path(f"/proc/{descendant}").exists())
                supervisor.terminate()
                self.assertEqual(supervisor.wait(timeout=5), 143)
                wait_for(lambda: not Path(f"/proc/{descendant}").exists())
            finally:
                if supervisor.poll() is None:
                    supervisor.kill()
                    supervisor.wait()

    def test_owner_sigkill_makes_supervisor_reap_double_fork_descendant(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            grandchild_pid = root / "grandchild.pid"
            child = root / "double_fork.py"
            child.write_text(
                """\
import os
import pathlib
import time

if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        pathlib.Path(os.environ["GRANDCHILD_PID"]).write_text(str(os.getpid()))
        time.sleep(30)
    os._exit(0)
time.sleep(30)
"""
            )
            owner = subprocess.Popen(["sleep", "30"])
            supervisor = subprocess.Popen(
                [
                    "python3",
                    str(SCRIPT_DIR / "process_supervisor.py"),
                    "run",
                    "--owner-pid",
                    str(owner.pid),
                    "--identity",
                    "owner-sigkill-double-fork",
                    "--",
                    "python3",
                    str(child),
                ],
                env={**os.environ, "GRANDCHILD_PID": str(grandchild_pid)},
            )
            try:
                wait_for(grandchild_pid.is_file)
                descendant = int(grandchild_pid.read_text())
                owner.kill()
                owner.wait(timeout=5)
                self.assertEqual(supervisor.wait(timeout=5), 143)
                wait_for(lambda: not Path(f"/proc/{descendant}").exists())
            finally:
                if owner.poll() is None:
                    owner.kill()
                    owner.wait()
                if supervisor.poll() is None:
                    supervisor.kill()
                    supervisor.wait()

    def test_snapshot_is_private_o_excl_mode_preserving_and_fd_verified(self):
        snapshot_contract = load_module("sealed_run_snapshot")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            agent = root / "astra"
            server = root / "astra-server"
            config = root / "source.json"
            control = root / "control.py"
            control.write_text("CONTROL = 1\n")
            task = root / "task"
            task.mkdir()
            (task / "environment").mkdir()
            executable = task / "tests" / "test.sh"
            executable.parent.mkdir()
            # Directory modes are part of the official task digest.  Use a
            # mode affected by the usual 022 umask to prove snapshotting
            # restores it after mkdir.
            executable.parent.chmod(0o775)
            executable.write_text("#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
            (task / "instruction.md").write_text("unchanged instruction")
            (task / "task.toml").write_text("[agent]\ntimeout_sec=900\n")
            (task / "relative-link").symlink_to("instruction.md")
            agent.write_bytes(b"agent-v1")
            server.write_bytes(b"server-v1")
            agent.chmod(0o755)
            server.chmod(0o755)
            source_task_digest = snapshot_contract._task_set_sha256([task])
            config.write_text(
                json.dumps(
                    {
                        "tasks": [{"path": str(task)}],
                        "agents": [
                            {
                                "env": {
                                    "ASTRA_HARNESS_TASK_SET_SHA256": source_task_digest
                                }
                            }
                        ],
                    }
                )
            )
            parent = root / "snapshots"
            parent.mkdir(mode=0o700)
            snapshot = snapshot_contract.create_snapshot(
                parent=parent,
                snapshot_id="fixed-id",
                agent=agent,
                server=server,
                config=config,
                tasks=[task],
                source_revision="a" * 40,
                consumer_root=Path("/proc/4242/fd/198"),
                control_base=root,
                control_paths=[control],
            )
            self.assertEqual(stat.S_IMODE(snapshot.root.stat().st_mode), 0o500)
            copied_test = snapshot.tasks[0] / "tests" / "test.sh"
            self.assertEqual(stat.S_IMODE(copied_test.stat().st_mode), 0o755)
            self.assertEqual(
                stat.S_IMODE((snapshot.tasks[0] / "tests").stat().st_mode), 0o775
            )
            self.assertTrue((snapshot.tasks[0] / "relative-link").is_symlink())
            self.assertEqual(
                os.readlink(snapshot.tasks[0] / "relative-link"), "instruction.md"
            )
            snapped_config = json.loads(snapshot.config.read_text())
            self.assertEqual(
                snapped_config["tasks"][0]["path"],
                "/proc/4242/fd/198/tasks/task",
            )
            self.assertEqual(
                snapped_config["agents"][0]["env"]["ASTRA_HARNESS_TASK_SET_SHA256"],
                snapshot_contract._task_set_sha256(snapshot.tasks),
            )
            self.assertEqual(snapshot.ledger["control_manifest"], ["control.py"])
            control.write_text("CONTROL = 2\n")
            self.assertEqual(
                (snapshot.root / "control" / "repo" / "control.py").read_text(),
                "CONTROL = 1\n",
            )
            agent.write_bytes(b"mutated source")
            snapshot.verify_open_ledger()
            verified = subprocess.run(
                [
                    "python3",
                    str(SCRIPT_DIR / "sealed_run_snapshot.py"),
                    "verify",
                    "--root-fd",
                    str(snapshot.root_fd),
                    "--ledger-fd",
                    str(snapshot.ledger_fd),
                ],
                pass_fds=(snapshot.root_fd, snapshot.ledger_fd),
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(verified.returncode, 0, verified.stderr)
            copied_agent = snapshot.agent
            snapshot.root.chmod(0o700)
            copied_agent.parent.chmod(0o700)
            copied_agent.chmod(0o755)
            copied_agent.unlink()
            copied_agent.write_bytes(b"agent-v1")
            copied_agent.chmod(0o555)
            copied_agent.parent.chmod(0o500)
            snapshot.root.chmod(0o500)
            with self.assertRaisesRegex(snapshot_contract.SnapshotError, "inode"):
                snapshot.verify_open_ledger()
            with self.assertRaises(FileExistsError):
                snapshot_contract.create_snapshot(
                    parent=parent,
                    snapshot_id="fixed-id",
                    agent=agent,
                    server=server,
                    config=config,
                    tasks=[task],
                    source_revision="a" * 40,
                    control_base=root,
                    control_paths=[control],
                )
            snapshot.close()

    def test_snapshot_task_set_identity_is_independent_of_parent_directory(self):
        snapshot_contract = load_module("sealed_run_snapshot")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            agent = root / "astra"
            server = root / "astra-server"
            control = root / "control.py"
            for binary in (agent, server):
                binary.write_bytes(binary.name.encode())
                binary.chmod(0o755)
            control.write_text("CONTROL = 1\n")
            # These names sort differently from their official parent paths,
            # exactly as content-addressed Harbor tasks do after sealing.
            tasks = []
            for parent_name, task_name in (
                ("gcode", "7dd2"),
                ("headless", "2039"),
                ("summary", "27b0"),
            ):
                task = root / "official" / parent_name / task_name
                task.mkdir(parents=True)
                (task / "task.toml").write_text("[agent]\ntimeout_sec=900\n")
                tasks.append(task)
            task_set = snapshot_contract._task_set_sha256(tasks)
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "tasks": [{"path": str(task)} for task in tasks],
                        "agents": [
                            {"env": {"ASTRA_HARNESS_TASK_SET_SHA256": task_set}}
                        ],
                    }
                )
            )
            parent = root / "snapshots"
            parent.mkdir()
            snapshot = snapshot_contract.create_snapshot(
                parent=parent,
                snapshot_id="task-set-order",
                agent=agent,
                server=server,
                config=config,
                tasks=tasks,
                source_revision="a" * 40,
                control_base=root,
                control_paths=[control],
            )
            self.assertEqual(
                task_set, snapshot_contract._task_set_sha256(snapshot.tasks)
            )
            snapshot.close()

    def test_snapshot_rejects_symlink_escape(self):
        snapshot_contract = load_module("sealed_run_snapshot")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / "task"
            task.mkdir()
            (task / "escape").symlink_to("../outside")
            for name in ("astra", "astra-server"):
                path = root / name
                path.write_bytes(name.encode())
                path.chmod(0o755)
            config = root / "config.json"
            config.write_text("{}")
            control = root / "control.py"
            control.write_text("CONTROL = 1\n")
            parent = root / "snapshots"
            parent.mkdir()
            with self.assertRaisesRegex(snapshot_contract.SnapshotError, "escape"):
                snapshot_contract.create_snapshot(
                    parent=parent,
                    snapshot_id="escape",
                    agent=root / "astra",
                    server=root / "astra-server",
                    config=config,
                    tasks=[task],
                    source_revision="a" * 40,
                    control_base=root,
                    control_paths=[control],
                )

    def test_create_exec_transfers_verified_fds_without_path_reopen(self):
        snapshot_contract = load_module("sealed_run_snapshot")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / "official-task"
            (task / "tests").mkdir(parents=True)
            test_file = task / "tests" / "test.sh"
            test_file.write_text("#!/bin/sh\nexit 0\n")
            test_file.chmod(0o751)
            (task / "task.toml").write_text("[agent]\ntimeout_sec=900\n")
            agent = root / "astra"
            server = root / "astra-server"
            for binary in (agent, server):
                binary.write_bytes(binary.name.encode())
                binary.chmod(0o755)
            task_set = snapshot_contract._task_set_sha256([task])
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "tasks": [{"path": str(task)}],
                        "agents": [
                            {"env": {"ASTRA_HARNESS_TASK_SET_SHA256": task_set}}
                        ],
                    }
                )
            )
            probe = root / "probe.py"
            probe.write_text(
                "import json, os, stat\n"
                "root='/proc/self/fd/198'\n"
                "cfg=json.load(open(root+'/config/final.json'))\n"
                "print(json.dumps({'pid':os.getpid(),'mode':stat.S_IMODE(os.stat(root+'/tasks/official-task/tests/test.sh').st_mode),'task_hash':cfg['agents'][0]['env']['ASTRA_HARNESS_TASK_SET_SHA256']}))\n"
            )
            parent = root / "snapshots"
            parent.mkdir()
            command = [
                "python3",
                str(SCRIPT_DIR / "sealed_run_snapshot.py"),
                "create-exec",
                "--parent",
                str(parent),
                "--snapshot-id",
                "exec-id",
                "--agent",
                str(agent),
                "--server",
                str(server),
                "--config",
                str(config),
                "--task",
                str(task),
                "--source-revision",
                "a" * 40,
                "--consumer-root",
                "/proc/self/fd/198",
                "--control-base",
                str(root),
                "--control",
                str(probe),
                "--",
                "python3",
                "/proc/self/fd/198/control/repo/probe.py",
            ]
            process = subprocess.Popen(
                command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
            )
            stdout, stderr = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0, stderr)
            result = json.loads(stdout)
            self.assertEqual(result["pid"], process.pid)
            self.assertEqual(result["mode"], 0o751)
            self.assertEqual(result["task_hash"], task_set)

    def test_domain_removes_large_snapshot_state_after_quiescence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            identity, port = self._unique_lifecycle_resource()
            child = root / "write_snapshot.py"
            child.write_text(
                "import os\n"
                "from pathlib import Path\n"
                "p=Path(os.environ['ASTRA_HARNESS_DOMAIN_STATE'])/'snapshot'\n"
                "p.mkdir()\n"
                "(p/'payload').write_bytes(b'x' * (8 * 1024 * 1024))\n"
            )
            completed = subprocess.run(
                [
                    "python3",
                    str(SCRIPT_DIR / "lifecycle_domain.py"),
                    "--database-identity",
                    identity,
                    "--gateway-port",
                    str(port),
                    "--state-parent",
                    str(root / "state"),
                    "--",
                    "python3",
                    str(child),
                ],
                env={**os.environ, "PYTHONPATH": str(SCRIPT_DIR)},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(list((root / "state").iterdir()), [])

    def test_runner_uses_broker_supervisors_and_snapshot_for_every_consumer(self):
        source = RUNNER_PATH.read_text()
        self.assertNotIn("flock", source)
        self.assertIn("lifecycle_broker.py", source)
        self.assertIn("process_supervisor.py", source)
        self.assertIn("sealed_run_snapshot.py", source)
        self.assertIn("lifecycle_domain.py", source)
        self.assertIn("recovery_environment.py", source)
        snapshot_create = source.index('exec python3 "$snapshot_script" create-exec')
        first_preflight = source.index('python3 "$preflight_script"')
        server_start = source.index(
            'python3 "$process_supervisor_script" run', first_preflight
        )
        harbor_start = source.index('harbor "${harbor_args[@]}"')
        self.assertLess(snapshot_create, first_preflight)
        self.assertLess(snapshot_create, server_start)
        self.assertLess(snapshot_create, harbor_start)
        self.assertIn("--snapshot-ledger-fd", source)
        self.assertIn(
            'sealed_harness_pythonpath="$(readlink -f "${snapshot_fd_root}/control/repo/crates/astra-test-harness")"',
            source,
        )
        self.assertIn(
            'env "PYTHONPATH=${sealed_harness_pythonpath}" harbor "${harbor_args[@]}"',
            source,
        )
        self.assertIn("--probe-verifier-readiness", source)
        self.assertIn(
            'preflight_evidence_directory="${repo_root}/target/harness-evidence/${snapshot_id}"',
            source,
        )
        self.assertIn(
            'preflight_stdout_log="${preflight_evidence_directory}/preflight.stdout.log"',
            source,
        )
        self.assertIn(
            'preflight_stderr_log="${preflight_evidence_directory}/preflight.stderr.log"',
            source,
        )
        self.assertIn('2> >(tee "$preflight_stderr_log" >&2)', source)
        snapshot_consumers = source[source.index("snapshot_fd_root=") :]
        self.assertNotIn('"$source_config"', snapshot_consumers)
        self.assertNotIn('"${source_tasks[@]}"', snapshot_consumers)
        self.assertIn('agent_bin="${snapshot_fd_root}/agent/astra"', snapshot_consumers)
        self.assertIn(
            'config="${snapshot_fd_root}/config/final.json"', snapshot_consumers
        )
        self.assertIn('snapshot_consumer_root="/proc/$$/fd/198"', source)
        self.assertIn('--consumer-root "$snapshot_consumer_root"', source)
        self.assertIn(
            'control_repo_root="${ASTRA_HARNESS_CONTROL_REPO:-$repo_root}"', source
        )
        self.assertIn("ASTRA_HARNESS_SNAPSHOT_ROOT_FD", source)
        self.assertIn("crates/runtime/src/server/sweeper_lease.rs", source)
        self.assertIn("crates/runtime/src/server/tool_invocation_compactor.rs", source)

    def test_runner_snapshot_closes_local_python_import_dependencies(self):
        source = RUNNER_PATH.read_text(encoding="utf-8")
        block = source.split("control_relative_paths=(", 1)[1].split("\n  )", 1)[0]
        manifest = {
            line.strip()
            for line in block.splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }
        harness_modules = {
            path.stem: path
            for path in SCRIPT_DIR.glob("*.py")
            if path.name != "__init__.py"
        }
        missing: list[str] = []
        for relative in sorted(manifest):
            if not relative.startswith("scripts/harness/") or not relative.endswith(
                ".py"
            ):
                continue
            tree = ast.parse(
                (SCRIPT_DIR.parent.parent / relative).read_text(encoding="utf-8")
            )
            imported: set[str] = set()
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    imported.update(alias.name.split(".", 1)[0] for alias in node.names)
                elif isinstance(node, ast.ImportFrom) and node.module:
                    imported.add(node.module.split(".", 1)[0])
            for module in sorted(imported & harness_modules.keys()):
                dependency = f"scripts/harness/{harness_modules[module].name}"
                if dependency not in manifest:
                    missing.append(f"{relative} -> {dependency}")
        self.assertEqual(
            missing, [], f"sealed control manifest misses imports: {missing}"
        )


if __name__ == "__main__":
    unittest.main()
