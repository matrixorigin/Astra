#!/usr/bin/env python3
"""Behavioral regression tests for the disposable verifier-readiness boundary."""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import importlib.util
import io
import json
import re
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path, PurePosixPath
from types import SimpleNamespace
from unittest import mock


MODULE_PATH = Path(__file__).with_name("verifier_readiness.py")
SPEC = importlib.util.spec_from_file_location("astra_verifier_readiness", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
readiness = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(readiness)


class FakeEnvironment:
    session_id = "astra-readiness-test"
    default_user = "verifier"

    def __init__(self) -> None:
        self.calls: list[object] = []

    async def upload_dir(self, *, source_dir, target_dir) -> None:
        self.calls.append(("upload_dir", str(source_dir), target_dir))

    async def exec(self, *, command, user, env):
        self.calls.append(("exec", command, user, env))
        completed = re.findall(
            rf"{re.escape(readiness.STEP_RECEIPT_PREFIX)}([0-9]+)=0", command
        )
        stdout = "\n".join(
            f"{readiness.STEP_RECEIPT_PREFIX}{index}=0" for index in completed
        )
        stdout += "\n" + "\n".join(
            f"{readiness.STEP_EXIT_STATUS_PREFIX}{index}=0" for index in completed
        )
        return type("Result", (), {"return_code": 0, "stdout": stdout, "stderr": ""})()

    async def prepare_logs_for_host(self) -> None:
        self.calls.append("prepare_logs")

    async def _run_docker_compose_command(self, command):
        self.calls.append(tuple(command))

    def _cleanup_mounts_compose_file(self) -> None:
        self.calls.append("cleanup_mounts")

    def _cleanup_resources_compose_file(self) -> None:
        self.calls.append("cleanup_resources")

    def _cleanup_env_compose_file(self) -> None:
        self.calls.append("cleanup_env")

    def _cleanup_egress_control_services_compose_file(self) -> None:
        self.calls.append("cleanup_egress")

    async def set_network_policy(self, policy) -> None:
        self.calls.append(("policy", policy))

    @staticmethod
    async def _collect_buffered_output(
        process, *, timeout_sec, stdin_data=None
    ):
        return await process.communicate(input=stdin_data)


class SourceEnvironment(FakeEnvironment):
    canonical_path = "/root/.local/bin/env"

    def __init__(
        self,
        source_content: bytes,
        identities: list[tuple[int, int, int, int]] | None = None,
        digests: list[str] | None = None,
    ) -> None:
        super().__init__()
        self.source_content = source_content
        self.identities = identities or [
            (2049, 1701, len(source_content), stat.S_IFREG | 0o600)
        ]
        self.stat_calls = 0
        self.digests = digests or [hashlib.sha256(source_content).hexdigest()]
        self.digest_calls = 0

    async def exec(self, *, command, user, env):
        if "readlink -f -- " in command:
            self.calls.append(("exec", command, user, env))
            return type(
                "Result",
                (),
                {"return_code": 0, "stdout": self.canonical_path + "\n", "stderr": ""},
            )()
        if " stat -Lc " in command:
            self.calls.append(("exec", command, user, env))
            identity = self.identities[min(self.stat_calls, len(self.identities) - 1)]
            self.stat_calls += 1
            return type(
                "Result",
                (),
                {
                    "return_code": 0,
                    "stdout": ":".join(
                        [*(str(value) for value in identity[:3]), f"{identity[3]:x}"]
                    )
                    + "\n",
                    "stderr": "",
                },
            )()
        if command.startswith("sha256sum -- "):
            self.calls.append(("exec", command, user, env))
            digest = self.digests[min(self.digest_calls, len(self.digests) - 1)]
            self.digest_calls += 1
            return type(
                "Result",
                (),
                {
                    "return_code": 0,
                    "stdout": f"{digest}  {self.canonical_path}\n",
                    "stderr": "",
                },
            )()
        return await super().exec(command=command, user=user, env=env)

    async def download_file(self, source_path, target_path) -> None:
        self.calls.append(("download_file", source_path, str(target_path)))
        Path(target_path).write_bytes(self.source_content)


UV_PATH_SOURCE = b"""#!/bin/sh
# add binaries to PATH if they aren't added yet
case ":${PATH}:" in
    *:"$HOME/.local/bin":*)
        ;;
    *)
        export PATH="$HOME/.local/bin:$PATH"
        ;;
esac
"""


class ReadinessBoundaryTests(unittest.TestCase):
    @staticmethod
    def _image_inspect_result(digest_character: str = "2"):
        digest = digest_character * 64
        return readiness.OwnedProcessResult(
            0,
            json.dumps(
                [
                    {
                        "Id": "sha256:" + digest,
                        "RepoDigests": ["example.invalid/task@sha256:" + digest],
                    }
                ]
            ),
            "",
        )

    def _probe_script(
        self, environment: FakeEnvironment, script: str
    ) -> dict[str, object]:
        paths = type("Paths", (), {"tests_dir": PurePosixPath("/tests")})()
        with tempfile.TemporaryDirectory() as directory:
            tests = Path(directory) / "tests"
            tests.mkdir()
            test = tests / "test.sh"
            test.write_text(script, encoding="utf-8")
            return asyncio.run(
                readiness._probe_verifier_container(
                    environment,
                    {"HTTP_PROXY": "http://proxy:8080"},
                    paths,
                    tests,
                    test,
                    False,
                    900.0,
                )
            )

    def test_strict_cleanup_runs_compose_down_and_all_local_cleanup(self):
        environment = FakeEnvironment()
        quiescence = mock.AsyncMock()
        with mock.patch.object(readiness, "_assert_project_quiescent", quiescence):
            asyncio.run(readiness._strict_delete_environment(environment))
        self.assertEqual(
            environment.calls,
            [
                "prepare_logs",
                ("down", "--rmi", "local", "--volumes", "--remove-orphans"),
                "cleanup_mounts",
                "cleanup_resources",
                "cleanup_env",
                "cleanup_egress",
            ],
        )
        quiescence.assert_awaited_once_with(environment)

    def test_cleanup_attempts_every_layer_and_preserves_first_failure(self):
        environment = FakeEnvironment()

        async def prepare_fail():
            environment.calls.append("prepare_failed")
            raise RuntimeError("prepare failed")

        async def down_fail(_command):
            environment.calls.append("down_failed")
            raise RuntimeError("compose failed")

        environment.prepare_logs_for_host = prepare_fail
        environment._run_docker_compose_command = down_fail
        quiescence = mock.AsyncMock(side_effect=RuntimeError("quiescence failed"))
        with (
            mock.patch.object(readiness, "_assert_project_quiescent", quiescence),
            self.assertRaises(readiness.ReadinessStageError) as raised,
        ):
            asyncio.run(readiness._strict_delete_environment(environment))
        self.assertEqual(raised.exception.stage, "cleanup prepare_logs")
        self.assertEqual(raised.exception.category, "runtime")
        self.assertIn("down_failed", environment.calls)
        self.assertEqual(
            environment.calls[-4:],
            ["cleanup_mounts", "cleanup_resources", "cleanup_env", "cleanup_egress"],
        )
        quiescence.assert_awaited_once_with(environment)

    def test_external_cancel_waits_for_cleanup_owner(self):
        environment = FakeEnvironment()
        prepare_started = asyncio.Event()
        release_prepare = asyncio.Event()

        async def prepare():
            environment.calls.append("prepare_started")
            prepare_started.set()
            await release_prepare.wait()
            environment.calls.append("prepare_finished")

        environment.prepare_logs_for_host = prepare
        quiescence = mock.AsyncMock()

        async def scenario():
            with mock.patch.object(readiness, "_assert_project_quiescent", quiescence):
                caller = asyncio.create_task(
                    readiness._await_cleanup_owner(environment)
                )
                await prepare_started.wait()
                caller.cancel()
                await asyncio.sleep(0)
                self.assertFalse(caller.done())
                caller.cancel()
                await asyncio.sleep(0)
                self.assertFalse(caller.done())
                release_prepare.set()
                with self.assertRaises(asyncio.CancelledError):
                    await caller

        asyncio.run(scenario())
        self.assertIn("prepare_finished", environment.calls)
        self.assertIn(
            ("down", "--rmi", "local", "--volumes", "--remove-orphans"),
            environment.calls,
        )
        self.assertEqual(
            environment.calls[-4:],
            ["cleanup_mounts", "cleanup_resources", "cleanup_env", "cleanup_egress"],
        )
        quiescence.assert_awaited_once_with(environment)

    def test_verifier_phase_switches_and_restores(self):
        environment = FakeEnvironment()

        async def invoke():
            async with readiness._verifier_phase(environment, "baseline", "verifier"):
                environment.calls.append("verify")

        asyncio.run(invoke())
        self.assertEqual(
            environment.calls,
            [("policy", "verifier"), "verify", ("policy", "baseline")],
        )

    def test_quiescence_rejects_leftover_volume(self):
        environment = FakeEnvironment()
        responses = iter(("", "", "volume-id\n"))

        async def run(*_args, **_kwargs):
            return readiness.OwnedProcessResult(0, next(responses), "")

        with mock.patch.object(readiness, "_run_owned_process", side_effect=run):
            with self.assertRaisesRegex(readiness.ReadinessError, "volumes remain"):
                asyncio.run(readiness._assert_project_quiescent(environment))

    def test_owned_subprocess_keeps_event_loop_live_and_reaps_on_cancel(self):
        async def ticker_scenario():
            ticks = 0
            process = asyncio.create_task(
                readiness._run_owned_process(
                    [sys.executable, "-c", "import time; time.sleep(0.06)"],
                    timeout_seconds=1,
                )
            )
            while not process.done():
                ticks += 1
                await asyncio.sleep(0.005)
            result = await process
            return ticks, result

        ticks, result = asyncio.run(ticker_scenario())
        self.assertEqual(result.returncode, 0)
        self.assertGreaterEqual(ticks, 5)

        with tempfile.TemporaryDirectory() as directory:
            pid_path = Path(directory) / "pid"

            async def cancel_scenario():
                process = asyncio.create_task(
                    readiness._run_owned_process(
                        [
                            sys.executable,
                            "-c",
                            "import os,pathlib,signal,sys,time; "
                            "signal.signal(signal.SIGTERM, lambda *_: None); "
                            "pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); "
                            "time.sleep(30)",
                            str(pid_path),
                        ],
                        timeout_seconds=60,
                    )
                )
                for _ in range(100):
                    if pid_path.exists():
                        break
                    await asyncio.sleep(0.005)
                self.assertTrue(pid_path.exists())
                process.cancel()
                await asyncio.sleep(0)
                self.assertFalse(process.done())
                process.cancel()
                await asyncio.sleep(0)
                self.assertFalse(process.done())
                with self.assertRaises(asyncio.CancelledError):
                    await process

            with mock.patch.object(
                readiness, "PROCESS_TERMINATION_GRACE_SECONDS", 0.05
            ):
                asyncio.run(cancel_scenario())
            pid = int(pid_path.read_text(encoding="utf-8"))
            self.assertFalse(Path(f"/proc/{pid}").exists())

    def test_compose_collector_reaps_before_start_network_or_down_cancel_returns(self):
        class ComposePaths:
            def __init__(self) -> None:
                self.started = asyncio.Event()
                self.pid = None

            @staticmethod
            async def _collect_buffered_output(
                process, *, timeout_sec, stdin_data=None
            ):
                return await process.communicate(input=stdin_data)

            async def _invoke(self):
                process = await asyncio.create_subprocess_exec(
                    sys.executable,
                    "-c",
                    "import signal,time; "
                    "signal.signal(signal.SIGTERM, lambda *_: None); "
                    "time.sleep(30)",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                self.pid = process.pid
                self.started.set()
                return await self._collect_buffered_output(
                    process, timeout_sec=None
                )

            async def start(self, *, force_build):
                return await self._invoke()

            async def set_network_policy(self, _policy):
                return await self._invoke()

            async def _run_docker_compose_command(self, _command):
                return await self._invoke()

        async def scenario(path):
            environment = ComposePaths()
            readiness._install_cancellation_safe_compose_collector(environment)
            if path == "start":
                operation = environment.start(force_build=False)
            elif path == "network":
                operation = environment.set_network_policy("verifier")
            else:
                operation = environment._run_docker_compose_command(["down"])
            caller = asyncio.create_task(operation)
            await environment.started.wait()
            caller.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await caller
            assert environment.pid is not None
            return environment.pid

        with mock.patch.object(
            readiness, "PROCESS_TERMINATION_GRACE_SECONDS", 0.02
        ):
            for path in ("start", "network", "down"):
                with self.subTest(path=path):
                    pid = asyncio.run(scenario(path))
                    self.assertFalse(Path(f"/proc/{pid}").exists())

    def test_compose_collector_preserves_normal_exec_result(self):
        async def scenario():
            process = await asyncio.create_subprocess_exec(
                sys.executable,
                "-c",
                "import sys; sys.stdout.write('ready'); sys.stderr.write('warn')",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            return await readiness._collect_buffered_output_cancellation_safe(
                process, timeout_sec=5
            )

        result = asyncio.run(scenario())
        self.assertEqual(result.return_code, 0)
        self.assertEqual(result.stdout, "ready")
        self.assertEqual(result.stderr, "warn")

    def test_image_materialization_timeouts_are_typed_and_stage_specific(self):
        async def scenario(failing_stage):
            async def run(command, *, timeout_seconds):
                operation = command[2]
                if operation == "inspect":
                    stage = (
                        "cache_inspect"
                        if "@sha256:" in command[-1]
                        else "post_inspect"
                    )
                else:
                    stage = "pull"
                if stage == failing_stage:
                    raise asyncio.TimeoutError
                if operation == "pull":
                    return readiness.OwnedProcessResult(0, "pulled", "")
                return self._image_inspect_result()

            with mock.patch.object(readiness, "_run_owned_process", side_effect=run):
                with self.assertRaises(readiness.ImageMaterializationError) as raised:
                    await readiness._inspect_image(
                        (
                            "example.invalid/task@sha256:" + "7" * 64
                            if failing_stage == "cache_inspect"
                            else "example.invalid/task:latest"
                        ),
                        pull_timeout_seconds=600,
                    )
            self.assertEqual(raised.exception.stage, failing_stage)
            self.assertEqual(raised.exception.kind, "timeout")
            self.assertTrue(raised.exception.detail)
            self.assertIn(f"stage={failing_stage}", str(raised.exception))

        for stage in ("cache_inspect", "pull", "post_inspect"):
            with self.subTest(stage=stage):
                asyncio.run(scenario(stage))

    def test_primary_registry_transport_uses_daemon_primary_mirror_and_proxy(self):
        async def run(command, *, timeout_seconds):
            self.assertEqual(timeout_seconds, readiness.IMAGE_INSPECT_TIMEOUT_SECONDS)
            if command[-1] == "{{json .RegistryConfig.Mirrors}}":
                return readiness.OwnedProcessResult(
                    0, json.dumps(["https://mirror.invalid/"]), ""
                )
            self.assertEqual(command[-1], "{{json .HTTPSProxy}}")
            return readiness.OwnedProcessResult(0, json.dumps("http://proxy.invalid"), "")

        endpoint = mock.Mock()
        with (
            mock.patch.object(readiness, "_run_owned_process", side_effect=run),
            mock.patch.object(readiness, "_probe_registry_endpoint", endpoint),
        ):
            asyncio.run(readiness._probe_primary_registry_transport())
        endpoint.assert_called_once_with(
            "https://mirror.invalid/v2/", "http://proxy.invalid"
        )

    def test_primary_registry_transport_failure_blocks_task_admission(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(3):
                path = root / f"task-{index}"
                path.mkdir()
                paths.append(path)
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "verifier": {"env": {key: "" for key in readiness.PROJECTION_KEYS}},
                        "tasks": [{"path": str(path)} for path in paths],
                    }
                ),
                encoding="utf-8",
            )
            transport = mock.AsyncMock(
                side_effect=readiness.ImageMaterializationError(
                    "registry_transport", "unreachable", "primary registry mirror tls probe failed"
                )
            )
            task_probe = mock.AsyncMock()
            with (
                mock.patch.object(
                    readiness, "_probe_primary_registry_transport", new=transport
                ),
                mock.patch.object(readiness, "probe_task_readiness", new=task_probe),
                self.assertRaises(readiness.ImageMaterializationError) as raised,
            ):
                asyncio.run(readiness.run(config, root / "ledger.json", root))
            self.assertEqual(
                (raised.exception.stage, raised.exception.kind),
                ("registry_transport", "unreachable"),
            )
            task_probe.assert_not_awaited()

    def test_image_materialization_nonzero_persists_no_raw_diagnostic(self):
        secret = "do-not-persist-this-secret"

        async def run(command, *, timeout_seconds):
            if command[2] == "pull":
                return readiness.OwnedProcessResult(
                    17,
                    "",
                    (
                        f"denied https://user:{secret}@registry.invalid/v2/"
                        f"?access_token={secret} " + "x" * 1000
                    ),
                )
            return self._image_inspect_result()

        with (
            mock.patch.object(readiness, "_run_owned_process", side_effect=run),
            self.assertRaises(readiness.ImageMaterializationError) as raised,
        ):
            asyncio.run(
                readiness._inspect_image(
                    "example.invalid/task:latest", pull_timeout_seconds=600
                )
            )
        self.assertEqual(raised.exception.stage, "pull")
        self.assertEqual(raised.exception.kind, "nonzero_exit")
        self.assertNotIn(secret, raised.exception.detail)
        self.assertEqual(
            raised.exception.detail,
            "docker exited with status 17; category=unspecified",
        )

    def test_image_diagnostic_metadata_survives_adversarial_secret_shapes(self):
        secrets = (
            "AUTHORIZATION_SECRET",
            "REGISTRY_AUTH_SECRET",
            "AMZ_CREDENTIAL_SECRET",
            "UNKNOWN_SHAPE_SECRET",
        )
        raw = json.dumps(
            {
                "Authorization": f"Bearer {secrets[0]}",
                "X-Registry-Auth": secrets[1],
                "X-Amz-Credential": secrets[2],
                "completely_unknown": secrets[3],
                "error": "unauthorized",
            }
        )
        detail = readiness._process_failure_detail(
            readiness.OwnedProcessResult(23, raw, raw)
        )
        for secret in secrets:
            self.assertNotIn(secret, detail)
        self.assertEqual(
            detail,
            "docker exited with status 23; category=authorization_failed",
        )

    def test_inspect_schema_errors_are_typed_at_the_exact_stage(self):
        malformed = readiness.OwnedProcessResult(
            0,
            json.dumps(
                [
                    {
                        "Id": "sha256:" + "2" * 64,
                        "RepoDigests": "example.invalid/task@sha256:" + "3" * 64,
                    }
                ]
            ),
            "",
        )

        async def cached_run(_command, *, timeout_seconds):
            return malformed

        with (
            mock.patch.object(
                readiness, "_run_owned_process", side_effect=cached_run
            ),
            self.assertRaises(readiness.ImageMaterializationError) as cached,
        ):
            asyncio.run(
                readiness._inspect_image(
                    "example.invalid/task@sha256:" + "3" * 64,
                    pull_timeout_seconds=600,
                )
            )
        self.assertEqual((cached.exception.stage, cached.exception.kind), (
            "cache_inspect",
            "invalid_output",
        ))

        async def mutable_run(command, *, timeout_seconds):
            if command[2] == "pull":
                return readiness.OwnedProcessResult(0, "pulled", "")
            return malformed

        with (
            mock.patch.object(
                readiness, "_run_owned_process", side_effect=mutable_run
            ),
            self.assertRaises(readiness.ImageMaterializationError) as post,
        ):
            asyncio.run(
                readiness._inspect_image(
                    "example.invalid/task:latest", pull_timeout_seconds=600
                )
            )
        self.assertEqual(
            (post.exception.stage, post.exception.kind),
            ("post_inspect", "invalid_output"),
        )

    def test_cached_digest_is_inspected_without_pull(self):
        exact = "example.invalid/task@sha256:" + "7" * 64
        process = mock.AsyncMock(return_value=self._image_inspect_result("7"))
        with mock.patch.object(readiness, "_run_owned_process", new=process):
            image_id, repo_digests, source = asyncio.run(
                readiness._inspect_image(exact, pull_timeout_seconds=600)
            )
        self.assertEqual(image_id, "sha256:" + "7" * 64)
        self.assertEqual(repo_digests, ["example.invalid/task@sha256:" + "7" * 64])
        self.assertEqual(source, "digest-pinned-cache")
        process.assert_awaited_once_with(
            ["docker", "image", "inspect", exact],
            timeout_seconds=readiness.IMAGE_INSPECT_TIMEOUT_SECONDS,
        )

    def test_mutable_image_pull_budget_and_concurrency_are_independent(self):
        active_pulls = 0
        max_active_pulls = 0
        pull_timeouts = []

        async def run(command, *, timeout_seconds):
            nonlocal active_pulls, max_active_pulls
            if command[2] == "pull":
                pull_timeouts.append(timeout_seconds)
                active_pulls += 1
                max_active_pulls = max(max_active_pulls, active_pulls)
                try:
                    await asyncio.sleep(0.005)
                finally:
                    active_pulls -= 1
                return readiness.OwnedProcessResult(0, "pulled", "")
            return self._image_inspect_result()

        async def scenario():
            semaphore = asyncio.Semaphore(readiness.IMAGE_MATERIALIZATION_CONCURRENCY)
            return await asyncio.gather(
                *(
                    readiness._inspect_image(
                        f"example.invalid/task-{index}:latest",
                        pull_timeout_seconds=600 + index,
                        materialization_semaphore=semaphore,
                    )
                    for index in range(4)
                )
            )

        with mock.patch.object(readiness, "_run_owned_process", side_effect=run):
            results = asyncio.run(scenario())
        self.assertEqual(max_active_pulls, 1)
        self.assertCountEqual(pull_timeouts, [600.0, 601.0, 602.0, 603.0])
        self.assertTrue(all(result[2] == "pulled" for result in results))

    def test_cancelled_pull_keeps_admission_until_owned_cleanup_finishes(self):
        pull_started = asyncio.Event()
        cleanup_started = asyncio.Event()
        release_cleanup = asyncio.Event()
        pull_calls = 0

        async def run(command, *, timeout_seconds):
            nonlocal pull_calls
            if command[2] != "pull":
                return self._image_inspect_result()
            pull_calls += 1
            if pull_calls > 1:
                return readiness.OwnedProcessResult(0, "pulled", "")
            pull_started.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cleanup_started.set()
                await release_cleanup.wait()
                raise

        async def scenario():
            semaphore = asyncio.Semaphore(1)
            first = asyncio.create_task(
                readiness._inspect_image(
                    "example.invalid/first:latest",
                    pull_timeout_seconds=600,
                    materialization_semaphore=semaphore,
                )
            )
            await pull_started.wait()
            first.cancel()
            await cleanup_started.wait()
            second = asyncio.create_task(
                readiness._inspect_image(
                    "example.invalid/second:latest",
                    pull_timeout_seconds=600,
                    materialization_semaphore=semaphore,
                )
            )
            await asyncio.sleep(0)
            self.assertEqual(pull_calls, 1)
            self.assertFalse(second.done())
            release_cleanup.set()
            with self.assertRaises(asyncio.CancelledError):
                await first
            self.assertEqual((await second)[2], "pulled")

        with mock.patch.object(readiness, "_run_owned_process", side_effect=run):
            asyncio.run(scenario())
        self.assertEqual(pull_calls, 2)

    def test_invalid_image_pull_budgets_fail_before_docker(self):
        for value in (False, 0, -1, float("inf"), float("nan"), "600"):
            with self.subTest(value=value):
                process = mock.AsyncMock()
                with (
                    mock.patch.object(readiness, "_run_owned_process", new=process),
                    self.assertRaisesRegex(
                        readiness.ReadinessError, "finite positive number"
                    ),
                ):
                    asyncio.run(
                        readiness._inspect_image(
                            "example.invalid/task:latest",
                            pull_timeout_seconds=value,
                        )
                    )
                process.assert_not_awaited()

    def test_compose_down_timeout_still_runs_local_and_quiescence_cleanup(self):
        environment = FakeEnvironment()

        async def stalled_down(_command):
            environment.calls.append("down_started")
            await asyncio.Event().wait()

        environment._run_docker_compose_command = stalled_down
        quiescence = mock.AsyncMock()
        with (
            mock.patch.object(readiness, "_assert_project_quiescent", quiescence),
            self.assertRaises(readiness.ReadinessError),
        ):
            asyncio.run(
                readiness._strict_delete_environment(
                    environment, cleanup_grace_seconds=0.01
                )
            )
        self.assertIn("down_started", environment.calls)
        self.assertEqual(
            environment.calls[-4:],
            ["cleanup_mounts", "cleanup_resources", "cleanup_env", "cleanup_egress"],
        )
        quiescence.assert_awaited_once_with(environment)

    def test_cleanup_layers_share_one_whole_deadline(self):
        environment = FakeEnvironment()

        async def stalled_prepare():
            environment.calls.append("prepare_started")
            await asyncio.Event().wait()

        async def stalled_down(_command):
            environment.calls.append("down_started")
            await asyncio.Event().wait()

        async def scenario():
            loop = asyncio.get_running_loop()
            started = loop.time()
            with self.assertRaises(readiness.ReadinessError):
                await readiness._strict_delete_environment(
                    environment, cleanup_grace_seconds=0.03
                )
            return loop.time() - started

        environment.prepare_logs_for_host = stalled_prepare
        environment._run_docker_compose_command = stalled_down
        quiescence = mock.AsyncMock(side_effect=asyncio.TimeoutError)
        with mock.patch.object(readiness, "_assert_project_quiescent", quiescence):
            elapsed = asyncio.run(scenario())
        self.assertLess(elapsed, 0.1)
        self.assertIn("prepare_started", environment.calls)
        self.assertIn("down_started", environment.calls)
        self.assertEqual(
            environment.calls[-4:],
            ["cleanup_mounts", "cleanup_resources", "cleanup_env", "cleanup_egress"],
        )
        quiescence.assert_awaited_once_with(environment)

    def test_network_transition_is_bounded_by_tail_deadline(self):
        environment = FakeEnvironment()

        async def stalled_policy(_policy):
            await asyncio.Event().wait()

        environment.set_network_policy = stalled_policy

        async def scenario():
            deadline = asyncio.get_running_loop().time() + 0.01
            async with readiness._verifier_phase(
                environment,
                "baseline",
                "verifier",
                tail_deadline=deadline,
            ):
                self.fail("stalled transition must not enter verifier phase")

        with self.assertRaisesRegex(readiness.ReadinessError, "network transition"):
            asyncio.run(scenario())

    def test_stage_and_cleanup_errors_never_render_secret_exceptions(self):
        secret = "DOWNSTREAM_EXCEPTION_SECRET"

        async def fail():
            raise RuntimeError(secret)

        async def stage_scenario():
            deadline = asyncio.get_running_loop().time() + 1
            await readiness._await_tail_stage(
                fail,
                stage="environment start",
                timeout_seconds=1,
                tail_deadline=deadline,
            )

        with self.assertRaises(readiness.ReadinessStageError) as stage:
            asyncio.run(stage_scenario())
        self.assertEqual(stage.exception.category, "runtime")
        self.assertNotIn(secret, str(stage.exception))

        network_environment = FakeEnvironment()

        async def fail_network(_policy):
            raise RuntimeError(secret)

        network_environment.set_network_policy = fail_network

        async def network_scenario():
            deadline = asyncio.get_running_loop().time() + 1
            async with readiness._verifier_phase(
                network_environment,
                "baseline",
                "verifier",
                tail_deadline=deadline,
            ):
                self.fail("failed network transition must not enter phase")

        with self.assertRaises(readiness.ReadinessStageError) as network:
            asyncio.run(network_scenario())
        self.assertEqual(network.exception.category, "runtime")
        self.assertNotIn(secret, str(network.exception))

        environment = FakeEnvironment()

        async def fail_down(_command):
            raise RuntimeError(secret)

        environment._run_docker_compose_command = fail_down
        quiescence = mock.AsyncMock()
        with (
            mock.patch.object(readiness, "_assert_project_quiescent", quiescence),
            self.assertRaises(readiness.ReadinessStageError) as cleanup,
        ):
            asyncio.run(readiness._strict_delete_environment(environment))
        self.assertNotIn(secret, str(cleanup.exception))

    def test_helper_unknown_exception_boundary_is_static(self):
        secret = "HELPER_BOUNDARY_SECRET"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config.json"
            config.write_text("{}")
            ledger = root / "ledger.json"
            stderr = io.StringIO()
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "verifier_readiness.py",
                        "--config",
                        str(config),
                        "--ledger",
                        str(ledger),
                        "--domain-state",
                        str(root),
                    ],
                ),
                mock.patch.object(
                    readiness,
                    "run",
                    new=mock.AsyncMock(side_effect=RuntimeError(secret)),
                ),
                contextlib.redirect_stderr(stderr),
            ):
                return_code = readiness.main()
        self.assertEqual(return_code, 78)
        self.assertEqual(
            stderr.getvalue(),
            "astra harness: verifier readiness failed: internal_error\n",
        )
        self.assertNotIn(secret, stderr.getvalue())

    def test_task_probe_preserves_primary_when_every_cleanup_layer_fails(self):
        class PrimaryFailure(RuntimeError):
            pass

        for failing_stage in ("start", "probe"):
            with (
                self.subTest(failing_stage=failing_stage),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                tests = root / "tests"
                environment_dir = root / "environment"
                tests.mkdir()
                environment_dir.mkdir()
                test_path = tests / "test.sh"
                test_path.write_text("pytest /tests/score.py\n", encoding="utf-8")
                task_environment = SimpleNamespace(
                    docker_image="example.invalid/task:latest",
                    os="linux",
                    build_timeout_sec=1,
                )
                task = SimpleNamespace(
                    has_steps=False,
                    short_name="cleanup-primary",
                    config=SimpleNamespace(
                        verifier=SimpleNamespace(env={}, timeout_sec=1, user="root"),
                        environment=task_environment,
                    ),
                    paths=SimpleNamespace(
                        tests_dir=tests,
                        environment_dir=environment_dir,
                        discovered_test_path_for=lambda _os: test_path,
                    ),
                )

                class FailingEnvironment(FakeEnvironment):
                    environment_id = "1" * 64

                    async def start(self, *, force_build):
                        self.calls.append("start")
                        if failing_stage == "start":
                            raise PrimaryFailure("primary start failure")

                    async def run_healthcheck(self):
                        self.calls.append("healthcheck")

                    async def prepare_logs_for_host(self):
                        self.calls.append("prepare_failed")
                        raise RuntimeError("prepare cleanup failure")

                    async def _run_docker_compose_command(self, _command):
                        self.calls.append("down_failed")
                        raise RuntimeError("down cleanup failure")

                environment = FailingEnvironment()
                network_plan = SimpleNamespace(
                    verifier_env_baseline=None,
                    agent_env_baseline="baseline",
                    verifier_phase="verifier",
                )
                paths = SimpleNamespace(
                    tests_dir=PurePosixPath("/tests"),
                    verifier_dir=PurePosixPath("/logs/verifier"),
                )
                quiescence = mock.AsyncMock(
                    side_effect=RuntimeError("quiescence cleanup failure")
                )
                probe = mock.AsyncMock(
                    side_effect=PrimaryFailure("primary probe failure")
                )
                inspect_image = mock.AsyncMock(
                    return_value=(
                        "sha256:" + "2" * 64,
                        ["example/task@sha256:" + "3" * 64],
                        "pulled",
                    )
                )
                with (
                    mock.patch.object(readiness, "Task", return_value=task),
                    mock.patch.object(
                        readiness, "resolve_task_verifier_mode", return_value="mode"
                    ),
                    mock.patch.object(
                        readiness,
                        "resolve_effective_verifier_env_config",
                        return_value=None,
                    ),
                    mock.patch.object(
                        readiness,
                        "_inspect_image",
                        new=inspect_image,
                    ),
                    mock.patch.object(
                        readiness,
                        "resolve_trial_network_plan",
                        return_value=network_plan,
                    ),
                    mock.patch.object(
                        readiness.EnvironmentFactory,
                        "create_environment_from_config",
                        return_value=environment,
                    ),
                    mock.patch.object(
                        readiness.EnvironmentPaths, "for_os", return_value=paths
                    ),
                    mock.patch.object(
                        readiness, "_install_compose_registration", return_value=None
                    ),
                    mock.patch.object(
                        readiness, "_assert_project_quiescent", quiescence
                    ),
                    mock.patch.object(readiness, "_probe_verifier_container", probe),
                    self.assertRaises(readiness.ReadinessStageError) as raised,
                ):
                    asyncio.run(
                        readiness.probe_task_readiness(
                            root,
                            {key: "" for key in readiness.PROJECTION_KEYS},
                            f"primary-{failing_stage}",
                            root,
                        )
                    )
                self.assertEqual(
                    raised.exception.stage,
                    (
                        "environment start"
                        if failing_stage == "start"
                        else "dependency setup probe"
                    ),
                )
                self.assertNotIn("primary", str(raised.exception))
                self.assertNotIn("prepare cleanup failure", str(raised.exception))
                inspect_image.assert_awaited_once_with(
                    "example.invalid/task:latest",
                    pull_timeout_seconds=1.0,
                    materialization_semaphore=None,
                )
                self.assertIn("down_failed", environment.calls)
                self.assertEqual(
                    environment.calls[-4:],
                    [
                        "cleanup_mounts",
                        "cleanup_resources",
                        "cleanup_env",
                        "cleanup_egress",
                    ],
                )
                quiescence.assert_awaited_once_with(environment)

    def test_container_probe_executes_complete_dependency_plan_without_scoring(self):
        environment = SourceEnvironment(UV_PATH_SOURCE)
        receipt = self._probe_script(
            environment,
            """#!/bin/bash
apt-get update
apt-get install -y curl
curl -LsSf https://example.invalid/installer.sh | sh
source $HOME/.local/bin/env
uvx \\
  -p 3.13 \\
  -w pytest==8.4.1 \\
  -w example-dependency==1.2.3 \\
  pytest /tests/scoring-sentinel.py
echo scored > /logs/verifier/reward.txt
""",
        )
        self.assertEqual(receipt["mode"], "executed")
        self.assertFalse(receipt["scoring_invoked"])
        self.assertEqual(receipt["budget_seconds"], 900.0)
        self.assertEqual(len(receipt["executions"]), 5)
        self.assertEqual(
            [invocation["kind"] for invocation in receipt["invocations"]],
            [
                "readability_probe",
                "dependency_setup",
                "source_resolve",
                "source_stat_before",
                "source_digest_before",
                "source_stat_after",
                "source_digest_after",
                "dependency_setup",
            ],
        )
        self.assertTrue(
            all(
                invocation["command_sha256"]
                != receipt["plan"]["scoring_command_sha256"]
                for invocation in receipt["invocations"]
            )
        )
        setup_commands = "\n".join(
            call[1]
            for call in environment.calls
            if isinstance(call, tuple) and call[0] == "exec"
        )
        self.assertIn("apt-get install -y curl", setup_commands)
        self.assertIn("installer.sh | sh", setup_commands)
        self.assertIn('export PATH=/root/.local/bin:"$PATH"', setup_commands)
        self.assertIn("example-dependency==1.2.3", setup_commands)
        self.assertIn("pytest --version", setup_commands)
        self.assertNotIn("scoring-sentinel", setup_commands)
        self.assertNotIn("reward.txt", setup_commands)
        self.assertEqual(
            receipt["sources"][0]["canonical_path"], "/root/.local/bin/env"
        )
        self.assertEqual(receipt["sources"][0]["device"], 2049)
        self.assertEqual(receipt["sources"][0]["inode"], 1701)
        self.assertEqual(
            receipt["sources"][0]["content_sha256"],
            hashlib.sha256(UV_PATH_SOURCE).hexdigest(),
        )

    def test_dependency_setup_failure_is_fail_closed_without_scoring(self):
        class FailingEnvironment(FakeEnvironment):
            async def exec(self, *, command, user, env):
                result = await super().exec(command=command, user=user, env=env)
                if readiness.STEP_RECEIPT_PREFIX in command:
                    result.return_code = 17
                return result

        environment = FailingEnvironment()
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                environment,
                "pip install pytest==8.4.1\npytest /tests/scoring-sentinel.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_batch_exec")
        setup_command = environment.calls[-1][1]
        self.assertIn("pip install pytest==8.4.1", setup_command)
        self.assertIn("pytest --version", setup_command)
        self.assertNotIn("scoring-sentinel", setup_command)

    def test_dependency_setup_timeout_cancels_before_scoring(self):
        class StalledEnvironment(FakeEnvironment):
            async def exec(self, *, command, user, env):
                self.calls.append(("exec", command, user, env))
                if readiness.STEP_RECEIPT_PREFIX in command:
                    await asyncio.Event().wait()
                return type(
                    "Result", (), {"return_code": 0, "stdout": "", "stderr": ""}
                )()

        environment = StalledEnvironment()
        paths = type("Paths", (), {"tests_dir": PurePosixPath("/tests")})()
        with tempfile.TemporaryDirectory() as directory:
            tests = Path(directory) / "tests"
            tests.mkdir()
            test = tests / "test.sh"
            test.write_text(
                "pip install pytest==8.4.1\npytest /tests/scoring-sentinel.py\n",
                encoding="utf-8",
            )

            async def invoke():
                await asyncio.wait_for(
                    readiness._probe_verifier_container(
                        environment,
                        {},
                        paths,
                        tests,
                        test,
                        False,
                        0.01,
                    ),
                    timeout=0.01,
                )

            with self.assertRaises(asyncio.TimeoutError):
                asyncio.run(invoke())
        setup_command = environment.calls[-1][1]
        self.assertIn("pytest --version", setup_command)
        self.assertNotIn("scoring-sentinel", setup_command)

    def test_setup_plan_preserves_uv_constraints_and_adapts_only_entrypoint(self):
        plan = readiness.build_dependency_setup_plan(
            """#!/bin/bash
apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y curl
curl -LsSf https://example.invalid/install.sh | sh
source $HOME/.local/bin/env
uvx \\
 -p 3.13 \\
 -w pytest==8.4.1 \\
 -w large-package==9.7 \\
 pytest --ctrf /logs/verifier/ctrf.json /tests/test_outputs.py
if [ $? -eq 0 ]; then
  echo 1
fi
""",
            Path("test.sh"),
        )
        self.assertEqual(plan.runner_family, "pytest")
        self.assertEqual(
            [step.kind for step in plan.steps],
            [
                "package_setup",
                "installer",
                "environment_source",
                "resolver_entrypoint",
            ],
        )
        resolver = plan.steps[-1].command
        self.assertIn("-p 3.13", resolver)
        self.assertIn("-w large-package==9.7", resolver)
        self.assertTrue(resolver.endswith("pytest --version"))
        self.assertNotIn("test_outputs.py", resolver)

    def test_uv_virtualenv_activation_is_creator_bound_and_never_sourced(self):
        creator = "uv venv -p 3.12 .tb"
        self.assertEqual(readiness._parse_uv_venv_target(creator), ".tb")
        activation = (
            b"VIRTUAL_ENV='/app/.tb'\n"
            b"export VIRTUAL_ENV\n"
            b"PATH=\"$VIRTUAL_ENV/bin:$PATH\"\n"
            b"export PATH\n"
        )
        digest = hashlib.sha256(activation).hexdigest()
        delta = readiness._parse_environment_source(
            activation,
            ".tb/bin/activate",
            "/app/.tb/bin/activate",
            creator_target=".tb",
            creator_digest=digest,
        )
        self.assertEqual(delta.path_prepend, ("/app/.tb/bin",))
        with self.assertRaises(readiness.ReadinessError):
            readiness._parse_environment_source(
                activation + b"echo scoring-sentinel\n",
                ".tb/bin/activate",
                "/app/.tb/bin/activate",
                creator_target=".tb",
                creator_digest=digest,
            )
        with self.assertRaises(readiness.ReadinessError):
            readiness._parse_environment_source(
                activation,
                ".tb/bin/activate",
                "/app/.tb/bin/activate",
            )

    def test_uv_virtualenv_plan_uses_typed_creator_step(self):
        plan = readiness.build_dependency_setup_plan(
            "uv venv -p 3.12 .tb\n"
            "source .tb/bin/activate\n"
            "uv pip install pytest==8.4.1\n"
            "uv run pytest /tests/test_outputs.py\n",
            Path("test.sh"),
        )
        self.assertEqual(
            [step.kind for step in plan.steps],
            ["venv_create", "environment_source", "resolver", "resolver_entrypoint"],
        )
        rendered = readiness._render_dependency_setup_batch(
            plan, 0, 1, readiness.EnvironmentDelta()
        )
        self.assertIn("test ! -e .tb", rendered)
        self.assertIn(readiness.VENV_DIGEST_PREFIX + "0", rendered)
        self.assertNotIn("source .tb/bin/activate", rendered)

    def test_generic_plan_stages_authoritative_nested_fixtures_and_revision(self):
        revision = "34bbbfdface3c18e5221aa7de6032d7220c6c6a1"
        with tempfile.TemporaryDirectory() as directory:
            tests = Path(directory) / "tests"
            nested = tests / "nested"
            nested.mkdir(parents=True)
            for name in ("metadata.csv", "image.png", "expected.csv"):
                (tests / name).write_bytes(name.encode())
            test = nested / "test.sh"
            script = (
                f"REV='{revision}'\n"
                "cp /tests/metadata.csv .\n"
                "cp /tests/image.png .\n"
                "cp /tests/expected.csv .\n"
                "uvx -w git+https://example.invalid/tool.git@${REV} "
                "pytest /tests/test_outputs.py\n"
            )
            test.write_text(script, encoding="utf-8")
            plan = readiness.build_dependency_setup_plan(
                script, test, tests_source_dir=tests
            )

        self.assertEqual(len(plan.fixtures), 3)
        self.assertEqual(plan.fixtures[0].source_relative, "metadata.csv")
        self.assertEqual(plan.steps[-1].kind, "resolver_entrypoint")
        self.assertIn(
            f"git+https://example.invalid/tool.git@{revision}",
            plan.steps[-1].command,
        )
        self.assertNotIn("${REV}", plan.steps[-1].command)
        receipt = json.dumps(plan.receipt_plan(), sort_keys=True)
        self.assertNotIn(revision, receipt)
        self.assertNotIn("REV", receipt)

    def test_static_binding_rejects_dynamic_or_ambiguous_forms(self):
        revision = "a" * 40
        rejected = (
            f"REV={revision} uvx -w git+x@${{REV}} pytest /tests/x.py\n",
            "uvx -w git+x@${REV} pytest /tests/x.py\n",
            f"REV={revision}\nREV={'b' * 40}\n"
            "uvx -w git+x@${REV} pytest /tests/x.py\n",
            f"REV={revision}\nuvx -w git+x@${{REV:-bad}} pytest /tests/x.py\n",
            "REV=sentinel.jwt.payload\n"
            "uvx -w git+https://example.invalid/x.git@${REV} pytest /tests/x.py\n",
        )
        for script in rejected:
            with self.subTest(script=script):
                with self.assertRaises(readiness.ReadinessContractError) as raised:
                    readiness.build_dependency_setup_plan(script, Path("test.sh"))
                self.assertEqual(
                    raised.exception.subcategory,
                    "plan_static_binding_disallowed",
                )
                self.assertNotIn("sentinel", str(raised.exception))

        with self.assertRaises(readiness.ReadinessError) as exported:
            readiness.build_dependency_setup_plan(
                f"export REV={revision}\n"
                "uvx -w git+https://example.invalid/x.git@${REV} "
                "pytest /tests/x.py\n",
                Path("test.sh"),
            )
        self.assertNotIn(revision, str(exported.exception))

        safe_export = readiness.build_dependency_setup_plan(
            "export DEBIAN_FRONTEND=noninteractive\npytest /tests/x.py\n",
            Path("test.sh"),
        )
        self.assertEqual(safe_export.steps[0].kind, "environment")
        self.assertEqual(
            safe_export.steps[0].command,
            "export DEBIAN_FRONTEND=noninteractive",
        )

    def test_fixture_stage_rejects_non_tests_source_and_unsafe_options(self):
        rejected = (
            "cp /tmp/input.csv .\npytest /tests/x.py\n",
            "cp -r /tests/input.csv .\npytest /tests/x.py\n",
            "cp /tests/../input.csv .\npytest /tests/x.py\n",
            "cp /tests/input.csv /tmp\npytest /tests/x.py\n",
        )
        for script in rejected:
            with self.subTest(script=script):
                with self.assertRaises(readiness.ReadinessContractError) as raised:
                    readiness.build_dependency_setup_plan(script, Path("test.sh"))
                self.assertEqual(
                    raised.exception.subcategory,
                    "plan_unclassified_pre_scoring_command",
                )

    def test_fixture_runtime_preserves_interleaved_setup_order(self):
        content = bytes(range(256)) * 735
        self.assertGreater(len(content), readiness.MAX_SOURCE_BYTES)

        class FixtureEnvironment(FakeEnvironment):
            def __init__(self, workdir="/app"):
                super().__init__()
                self.workdir = workdir

            async def exec(self, *, command, user, env):
                if command == "pwd -P":
                    self.calls.append(("workdir", command))
                    return SimpleNamespace(
                        return_code=0, stdout=f"{self.workdir}\n", stderr=""
                    )
                if command.startswith("test ! -e "):
                    self.calls.append(("destination", command))
                    return SimpleNamespace(return_code=0, stdout="", stderr="")
                if "stat -Lc" in command:
                    self.calls.append(("stat", command))
                    return SimpleNamespace(
                        return_code=0,
                        stdout=f"1:2:{len(content)}:{stat.S_IFREG | 0o600:x}\n",
                        stderr="",
                    )
                if command.startswith("sha256sum -- "):
                    self.calls.append(("digest", command))
                    digest = hashlib.sha256(content).hexdigest()
                    return SimpleNamespace(
                        return_code=0,
                        stdout=f"{digest}  /app/input.csv\n",
                        stderr="",
                    )
                return await super().exec(command=command, user=user, env=env)

            async def upload_file(self, source_path, target_path):
                self.calls.append(("upload_file", str(source_path), target_path))

        for script, expected_batch_count in (
            (
                "pip install before==1\ncp /tests/input.csv .\n"
                "pip install after==1\npytest /tests/score.py\n",
                2,
            ),
            (
                "cp /tests/input.csv .\npip install after==1\n"
                "pytest /tests/score.py\n",
                1,
            ),
        ):
            with self.subTest(script=script), tempfile.TemporaryDirectory() as directory:
                tests = Path(directory) / "tests"
                tests.mkdir()
                (tests / "input.csv").write_bytes(content)
                test = tests / "test.sh"
                test.write_text(script, encoding="utf-8")
                environment = FixtureEnvironment()
                receipt = asyncio.run(
                    readiness._probe_verifier_container(
                        environment,
                        {},
                        readiness.EnvironmentPaths(),
                        tests,
                        test,
                        False,
                        900.0,
                    )
                )
                labels = []
                for call in environment.calls:
                    if call[0] == "exec" and readiness.STEP_RECEIPT_PREFIX in call[1]:
                        labels.append("batch")
                    elif call[0] in {
                        "workdir",
                        "destination",
                        "upload_file",
                        "stat",
                        "digest",
                    }:
                        labels.append(call[0])
                fixture_index = labels.index("workdir")
                self.assertEqual(
                    labels[fixture_index : fixture_index + 5],
                    ["workdir", "destination", "upload_file", "stat", "digest"],
                )
                self.assertEqual(labels.count("batch"), expected_batch_count)
                if script.startswith("pip install before"):
                    self.assertEqual(labels[fixture_index - 1], "batch")
                self.assertEqual(labels[-1], "batch")
                expected_step_index = (
                    1 if script.startswith("pip install before") else 0
                )
                self.assertEqual(
                    receipt["fixtures"][0]["step_index"], expected_step_index
                )
                self.assertEqual(
                    receipt["fixtures"][0]["content_bytes"], len(content)
                )
                self.assertEqual(
                    receipt["fixtures"][0]["content_sha256"],
                    hashlib.sha256(content).hexdigest(),
                )

    def test_fixture_runtime_fails_closed_on_unsafe_remote_state(self):
        content = b"fixture-content"

        class UnsafeFixtureEnvironment(FakeEnvironment):
            def __init__(self, mode):
                super().__init__()
                self.mode = mode
                self.local_source = None

            async def exec(self, *, command, user, env):
                if command == "pwd -P":
                    if self.mode == "source_change" and self.local_source is not None:
                        self.local_source.write_bytes(b"changed")
                    workdir = "/tests" if self.mode == "protected" else "/app"
                    return SimpleNamespace(
                        return_code=0, stdout=f"{workdir}\n", stderr=""
                    )
                if command.startswith("test ! -e "):
                    return SimpleNamespace(
                        return_code=1 if self.mode == "existing" else 0,
                        stdout="",
                        stderr="",
                    )
                if "stat -Lc" in command:
                    return SimpleNamespace(
                        return_code=0,
                        stdout=f"1:2:{len(content)}:{stat.S_IFREG | 0o600:x}\n",
                        stderr="",
                    )
                if command.startswith("sha256sum -- "):
                    digest = (
                        "f" * 64
                        if self.mode == "digest_mismatch"
                        else hashlib.sha256(content).hexdigest()
                    )
                    return SimpleNamespace(
                        return_code=0, stdout=f"{digest}  target\n", stderr=""
                    )
                return await super().exec(command=command, user=user, env=env)

            async def upload_file(self, source_path, target_path):
                self.calls.append(("upload_file", str(source_path), target_path))

        for mode in ("protected", "existing", "digest_mismatch", "source_change"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                tests = Path(directory) / "tests"
                tests.mkdir()
                source = tests / "input.csv"
                source.write_bytes(content)
                test = tests / "test.sh"
                test.write_text(
                    "cp /tests/input.csv .\npytest /tests/score.py\n",
                    encoding="utf-8",
                )
                environment = UnsafeFixtureEnvironment(mode)
                environment.local_source = source
                with self.assertRaises(readiness.ReadinessError):
                    asyncio.run(
                        readiness._probe_verifier_container(
                            environment,
                            {},
                            readiness.EnvironmentPaths(),
                            tests,
                            test,
                            False,
                            900.0,
                        )
                    )

    def test_dynamic_or_executable_environment_sources_fail_before_scoring(self):
        dynamic = FakeEnvironment()
        with self.assertRaisesRegex(readiness.ReadinessError, "dynamic.*source"):
            self._probe_script(
                dynamic,
                "source $DYNAMIC_ENV\npytest /tests/scoring-sentinel.py\n",
            )
        dynamic_commands = [
            call[1]
            for call in dynamic.calls
            if isinstance(call, tuple) and call[0] == "exec"
        ]
        self.assertEqual(len(dynamic_commands), 1)

        malicious_content = b"export SAFE=value\nscoring-sentinel\n"
        malicious = SourceEnvironment(malicious_content)
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                malicious,
                "source $HOME/.local/bin/env\npytest /tests/scoring-sentinel.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_source_policy")

        malicious_commands = [
            call[1]
            for call in malicious.calls
            if isinstance(call, tuple) and call[0] == "exec"
        ]
        self.assertNotIn("scoring-sentinel", "\n".join(malicious_commands))
        self.assertNotIn("export SAFE", "\n".join(malicious_commands))

        with self.assertRaisesRegex(readiness.ReadinessError, "restricted.*PATH"):
            readiness._parse_environment_source(
                b"LOCAL=literal\nNAME=literal\nexport NAME\n",
                "/tmp/env",
                "/tmp/env",
            )

    def test_sensitive_environment_and_tracing_never_reach_environment(self):
        unsafe_scripts = (
            "export API_TOKEN=sentinel-secret\npytest /tests/score.py\n",
            "export SSH_KEY_PATH=/tmp/sentinel-secret\npytest /tests/score.py\n",
            "AUTH_SECRET=sentinel-secret apt-get update\npytest /tests/score.py\n",
            "CI_JOB_JWT=sentinel-jwt apt-get update\npytest /tests/score.py\n",
            "DATABASE_URL=postgres://user:sentinel@db/name apt-get update\n"
            "pytest /tests/score.py\n",
            "FOO=sentinel-arbitrary apt-get update\npytest /tests/score.py\n",
            "apt-get update && FOO=sentinel-chain apt-get install -y curl\n"
            "pytest /tests/score.py\n",
            "apt-get update && CI_JOB_JWT=sentinel-chain apt-get install -y curl\n"
            "pytest /tests/score.py\n",
            "curl -fsSL https://example.invalid/install | FOO=sentinel-pipe sh\n"
            "pytest /tests/score.py\n",
            "curl -fsSL https://example.invalid/install | "
            "CI_JOB_JWT=sentinel-pipe sh\npytest /tests/score.py\n",
            "set\npytest /tests/score.py\n",
            "set -x\npytest /tests/score.py\n",
            "set -o xtrace\npytest /tests/score.py\n",
            "source $HOME/.local/bin/env\n"
            'if [ -n "$PATH" ]; then\n'
            '  echo "$PATH"\n'
            "fi\n"
            "pytest /tests/score.py\n",
        )
        for script in unsafe_scripts:
            environment = FakeEnvironment()
            with (
                self.subTest(script=script),
                self.assertRaises(readiness.ReadinessError),
            ):
                self._probe_script(environment, script)
            commands = [
                call[1]
                for call in environment.calls
                if isinstance(call, tuple) and call[0] == "exec"
            ]
            self.assertEqual(len(commands), 1)
            self.assertNotIn("sentinel", "\n".join(commands))
            self.assertNotIn("/tests/score.py", "\n".join(commands))

        safe = readiness.build_dependency_setup_plan(
            "set -euo pipefail\npytest /tests/score.py\n", Path("test.sh")
        )
        self.assertEqual(safe.steps[0].command, "set -euo pipefail")
        public_assignment = readiness.build_dependency_setup_plan(
            "DEBIAN_FRONTEND=noninteractive apt-get install -y curl\n"
            "pytest /tests/score.py\n",
            Path("test.sh"),
        )
        self.assertEqual(public_assignment.steps[0].kind, "package_setup")

    def test_source_credentials_are_never_rendered_or_logged(self):
        secret = "sentinel-source-credential"
        environment = SourceEnvironment(f"export ACCESS_TOKEN={secret}\n".encode())
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                environment,
                "source $HOME/.local/bin/env\npytest /tests/score.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_source_policy")
        commands = [
            call[1]
            for call in environment.calls
            if isinstance(call, tuple) and call[0] == "exec"
        ]
        self.assertNotIn(secret, "\n".join(commands))
        self.assertNotIn("ACCESS_TOKEN", "\n".join(commands))
        self.assertNotIn("/tests/score.py", "\n".join(commands))

    def test_stateful_setup_after_environment_source_stays_in_final_batch(self):
        environment = SourceEnvironment(UV_PATH_SOURCE)
        result = self._probe_script(
            environment,
            "source $HOME/.local/bin/env\n"
            "cd /tests\n"
            "uvx -p 3.13 -w pytest==8.3.4 pytest test_outputs.py\n",
        )

        self.assertEqual(len(result["sources"]), 1)
        self.assertEqual(len(result["batches"]), 1)
        setup_commands = [
            call[1]
            for call in environment.calls
            if isinstance(call, tuple)
            and call[0] == "exec"
            and readiness.STEP_RECEIPT_PREFIX in call[1]
        ]
        self.assertEqual(len(setup_commands), 1)
        self.assertIn("cd /tests", setup_commands[0])
        self.assertIn("pytest --version", setup_commands[0])

    def test_stateful_setup_before_environment_source_remains_unavailable(self):
        environment = SourceEnvironment(UV_PATH_SOURCE)
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                environment,
                "cd /tests\n"
                "source $HOME/.local/bin/env\n"
                "pytest test_outputs.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_source_policy")

        async def propagate():
            async def reject():
                raise raised.exception

            return await readiness._await_tail_stage(
                reject,
                stage="dependency setup probe",
                timeout_seconds=1.0,
                tail_deadline=asyncio.get_running_loop().time() + 1.0,
            )

        with self.assertRaises(readiness.ReadinessStageError) as staged:
            asyncio.run(propagate())
        self.assertEqual(staged.exception.subcategory, "dependency_source_policy")
        flattened = readiness._flatten_task_probe_error(
            task_index=0,
            task_name="source-task",
            error=staged.exception,
        )
        self.assertEqual(flattened.subcategory, "dependency_source_policy")

    def test_source_requires_stable_regular_non_symlink_identity(self):
        size = len(UV_PATH_SOURCE)
        unsafe_identities = (
            [(2049, 1701, size, stat.S_IFDIR | 0o700)],
            [(2049, 1701, size, stat.S_IFIFO | 0o600)],
            [
                (2049, 1701, size, stat.S_IFREG | 0o600),
                (2049, 1702, size, stat.S_IFREG | 0o600),
            ],
        )
        for identities in unsafe_identities:
            environment = SourceEnvironment(UV_PATH_SOURCE, identities)
            with (
                self.subTest(identities=identities),
                self.assertRaises(readiness.ReadinessError),
            ):
                self._probe_script(
                    environment,
                    "source $HOME/.local/bin/env\npytest /tests/score.py\n",
                )
            commands = [
                call[1]
                for call in environment.calls
                if isinstance(call, tuple) and call[0] == "exec"
            ]
            self.assertNotIn("/tests/score.py", "\n".join(commands))

        rewritten = SourceEnvironment(
            UV_PATH_SOURCE,
            identities=[(2049, 1701, size, stat.S_IFREG | 0o600)],
            digests=["0" * 64, hashlib.sha256(UV_PATH_SOURCE).hexdigest()],
        )
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                rewritten,
                "source $HOME/.local/bin/env\npytest /tests/score.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_source_digest")
        rewritten_commands = [
            call[1]
            for call in rewritten.calls
            if isinstance(call, tuple) and call[0] == "exec"
        ]
        self.assertNotIn("/tests/score.py", "\n".join(rewritten_commands))

        class SymlinkEnvironment(SourceEnvironment):
            async def exec(self, *, command, user, env):
                if "test ! -L" in command:
                    self.calls.append(("exec", command, user, env))
                    return type(
                        "Result",
                        (),
                        {"return_code": 1, "stdout": "", "stderr": "symlink"},
                    )()
                return await super().exec(command=command, user=user, env=env)

        symlink = SymlinkEnvironment(UV_PATH_SOURCE)
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                symlink,
                "source $HOME/.local/bin/env\npytest /tests/score.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_source_resolve")
        self.assertFalse(
            any(
                isinstance(call, tuple) and call[0] == "download_file"
                for call in symlink.calls
            )
        )
        resolve = readiness._source_resolve_command("$HOME/.local/bin/env")
        self.assertIn("test ! -L", resolve)
        self.assertIn("readlink -f", resolve)

    def test_source_stat_command_round_trips_real_coreutils_output(self):
        with tempfile.NamedTemporaryFile() as source:
            command = readiness._source_stat_command(source.name)
            completed = subprocess.run(
                ["bash", "-c", command],
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            result = SimpleNamespace(
                return_code=completed.returncode,
                stdout=completed.stdout,
                stderr=completed.stderr,
            )
            identity = readiness._parse_source_file_identity(result)
            observed = Path(source.name).stat()
            self.assertEqual(
                identity, (observed.st_dev, observed.st_ino, observed.st_size)
            )
            self.assertNotIn("\\t", completed.stdout)

        oversized = SimpleNamespace(
            return_code=0,
            stdout=f"1:2:{readiness.MAX_SOURCE_BYTES + 1}:{stat.S_IFREG | 0o600:x}\n",
            stderr="",
        )
        self.assertEqual(
            readiness._parse_regular_file_identity(oversized),
            (1, 2, readiness.MAX_SOURCE_BYTES + 1),
        )
        with self.assertRaisesRegex(readiness.ReadinessError, "identity"):
            readiness._parse_source_file_identity(oversized)

    def test_oversized_environment_source_remains_fail_closed(self):
        content = UV_PATH_SOURCE + b"#" * readiness.MAX_SOURCE_BYTES
        environment = SourceEnvironment(content)
        with self.assertRaises(readiness.DependencyProbeError) as raised:
            self._probe_script(
                environment,
                "source $HOME/.local/bin/env\npytest /tests/score.py\n",
            )
        self.assertEqual(raised.exception.subcategory, "dependency_source_stat")

    def test_guard_grammar_never_passes_hidden_scorer_to_environment(self):
        malicious_guards = (
            "if [ -n x ] || scoring-sentinel; then\n  echo ok\nfi",
            "if test -n x > /tmp/gate; then\n  echo ok\nfi",
            "if [ -n x ]; then\n  echo ok; scoring-sentinel\nfi",
            "if [ -n x ]; then\n  echo $(scoring-sentinel)\nfi",
            'if [ -n "${PS1@P}" ]; then\n  echo ok\nfi',
        )
        for guard in malicious_guards:
            environment = FakeEnvironment()
            with self.subTest(guard=guard):
                with self.assertRaises(readiness.ReadinessError):
                    self._probe_script(
                        environment,
                        f"{guard}\npytest /tests/official-score.py\n",
                    )
            exec_commands = [
                call[1]
                for call in environment.calls
                if isinstance(call, tuple) and call[0] == "exec"
            ]
            self.assertEqual(len(exec_commands), 1)
            self.assertNotIn("scoring-sentinel", exec_commands[0])
            self.assertNotIn("official-score.py", exec_commands[0])

    def test_safe_guard_and_javascript_resolver_have_non_scoring_entrypoints(self):
        guarded = readiness.build_dependency_setup_plan(
            """if [ "$PWD" = "/" ]; then
  echo invalid-workdir
  exit 1
fi
pytest /tests/official-score.py
""",
            Path("test.sh"),
        )
        self.assertEqual(
            [step.kind for step in guarded.steps],
            ["environment_guard", "minimal_entrypoint"],
        )
        self.assertEqual(guarded.steps[-1].command, "pytest --version")

        for manager in ("npm", "pnpm", "yarn"):
            with self.subTest(manager=manager):
                plan = readiness.build_dependency_setup_plan(
                    f"{manager} install\n{manager} test\n", Path("test.sh")
                )
                self.assertEqual(plan.runner_family, f"{manager}_test")
                self.assertEqual(
                    [step.kind for step in plan.steps],
                    ["resolver", "minimal_entrypoint"],
                )
                self.assertEqual(plan.steps[-1].command, f"{manager} list --depth=0")
                rendered = readiness._render_dependency_setup_command(plan)
                self.assertNotIn(f"{manager} test", rendered)

    def test_run_bounds_concurrency_and_preserves_record_order(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(7):
                path = root / f"task-{index}"
                path.mkdir()
                paths.append(path)
            projection = {key: "" for key in readiness.PROJECTION_KEYS}
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "verifier": {"env": projection},
                        "tasks": [{"path": str(path)} for path in paths],
                    }
                ),
                encoding="utf-8",
            )
            ledger = root / "ledger.json"
            active = 0
            max_active = 0

            async def probe(path, _projection, session, _state_dir, _image_semaphore):
                nonlocal active, max_active
                active += 1
                max_active = max(max_active, active)
                try:
                    await asyncio.sleep((7 - int(path.name.rsplit("-", 1)[1])) / 1000)
                    return {"task": path.name, "session": session}
                finally:
                    active -= 1

            with mock.patch.object(
                readiness, "probe_task_readiness", side_effect=probe
            ), mock.patch.object(
                readiness, "_probe_primary_registry_transport", new=mock.AsyncMock()
            ):
                asyncio.run(readiness.run(config, ledger, root, max_concurrency=2))
            payload = json.loads(ledger.read_text(encoding="utf-8"))
            self.assertEqual(max_active, 2)
            self.assertEqual(
                [record["task"] for record in payload["records"]],
                [path.name for path in paths],
            )
            self.assertEqual(
                len({record["session"] for record in payload["records"]}), 7
            )

    def test_task_probe_error_flattening_is_closed_and_secret_free(self):
        cleanup = readiness.ReadinessStageError(
            "cleanup compose_down", "exception", "runtime"
        )
        flattened_cleanup = readiness._flatten_task_probe_error(
            task_index=3,
            task_name="safe-task",
            error=cleanup,
        )
        self.assertEqual(
            (
                flattened_cleanup.task_index,
                flattened_cleanup.task_name,
                flattened_cleanup.stage,
                flattened_cleanup.kind,
                flattened_cleanup.category,
            ),
            (3, "safe-task", "cleanup compose_down", "exception", "runtime"),
        )

        sentinel = "IMAGE_PULL_SENTINEL_SECRET"
        image_error = readiness.ImageMaterializationError(
            "pull", "nonzero_exit", sentinel
        )
        flattened_image = readiness._flatten_task_probe_error(
            task_index=4,
            task_name="image-task",
            error=image_error,
        )
        self.assertEqual(
            (flattened_image.stage, flattened_image.kind, flattened_image.category),
            ("pull", "nonzero_exit", "image_materialization"),
        )
        self.assertNotIn(sentinel, str(flattened_image))

        unknown = readiness._flatten_task_probe_error(
            task_index=5,
            task_name="unknown-task",
            error=RuntimeError("UNKNOWN_SENTINEL_SECRET"),
        )
        self.assertEqual(
            (unknown.stage, unknown.kind, unknown.category),
            ("task probe", "exception", "internal"),
        )
        self.assertNotIn("UNKNOWN_SENTINEL_SECRET", str(unknown))

    def test_task_probe_error_main_renders_only_closed_fields(self):
        task_error = readiness.TaskProbeStageError(
            task_index=2,
            task_name="safe-task",
            stage="pull",
            kind="nonzero_exit",
            category="image_materialization",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config.json"
            config.write_text("{}")
            stderr = io.StringIO()
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "verifier_readiness.py",
                        "--config",
                        str(config),
                        "--ledger",
                        str(root / "ledger.json"),
                        "--domain-state",
                        str(root),
                    ],
                ),
                mock.patch.object(
                    readiness,
                    "run",
                    new=mock.AsyncMock(side_effect=task_error),
                ),
                contextlib.redirect_stderr(stderr),
            ):
                return_code = readiness.main()
        self.assertEqual(return_code, 78)
        self.assertEqual(
            stderr.getvalue(),
            "astra harness: verifier readiness failed: "
            "verifier readiness task failed [task_index=2, "
            "task_name=safe-task, stage=pull, kind=nonzero_exit, "
            "category=image_materialization]\n",
        )

    def test_contract_subcategory_survives_run_and_main_without_raw_detail(self):
        sentinel = "STATIC_BINDING_SENTINEL_SECRET"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(3):
                path = root / f"task-{index}"
                path.mkdir()
                paths.append(path)
            projection = {key: "" for key in readiness.PROJECTION_KEYS}
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "verifier": {"env": projection},
                        "tasks": [{"path": str(path)} for path in paths],
                    }
                ),
                encoding="utf-8",
            )

            async def probe(path, *_args):
                async def reject():
                    error = readiness.ReadinessContractError(
                        "plan_static_binding_disallowed"
                    )
                    error.add_note(sentinel)
                    raise error

                return await readiness._await_tail_stage(
                    reject,
                    stage="dependency setup probe",
                    timeout_seconds=1.0,
                    tail_deadline=asyncio.get_running_loop().time() + 1.0,
                )

            with (
                mock.patch.object(readiness, "probe_task_readiness", side_effect=probe),
                mock.patch.object(
                    readiness,
                    "_probe_primary_registry_transport",
                    new=mock.AsyncMock(),
                ),
                self.assertRaises(readiness.TaskProbeStageError) as raised,
            ):
                asyncio.run(
                    readiness.run(
                        config, root / "ledger.json", root, max_concurrency=1
                    )
                )
            task_error = raised.exception
            self.assertEqual(
                task_error.subcategory, "plan_static_binding_disallowed"
            )
            self.assertNotIn(sentinel, str(task_error))

            stderr = io.StringIO()
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "verifier_readiness.py",
                        "--config",
                        str(config),
                        "--ledger",
                        str(root / "other-ledger.json"),
                        "--domain-state",
                        str(root),
                    ],
                ),
                mock.patch.object(
                    readiness,
                    "run",
                    new=mock.AsyncMock(side_effect=task_error),
                ),
                contextlib.redirect_stderr(stderr),
            ):
                return_code = readiness.main()
        self.assertEqual(return_code, 78)
        self.assertIn("subcategory=plan_static_binding_disallowed", stderr.getvalue())
        self.assertNotIn(sentinel, stderr.getvalue())

    def test_fixture_runtime_subcategory_survives_stage_flatten_and_main(self):
        sentinel = "FIXTURE_RUNTIME_SENTINEL_SECRET"
        content = b"fixture"

        class FailingFixtureEnvironment(FakeEnvironment):
            def __init__(self, mode):
                super().__init__()
                self.mode = mode

            async def upload_file(self, source_path, target_path):
                if self.mode == "upload":
                    raise RuntimeError(sentinel)

            async def exec(self, *, command, user, env):
                if command == "pwd -P":
                    return SimpleNamespace(return_code=0, stdout="/app\n", stderr="")
                if command.startswith("test ! -e "):
                    return SimpleNamespace(return_code=0, stdout="", stderr="")
                if "stat -Lc" in command:
                    if self.mode == "stat":
                        raise RuntimeError(sentinel)
                    return SimpleNamespace(
                        return_code=0,
                        stdout=f"1:2:{len(content)}:{stat.S_IFREG | 0o600:x}\n",
                        stderr="",
                    )
                if command.startswith("sha256sum -- "):
                    digest = hashlib.sha256(content).hexdigest()
                    return SimpleNamespace(
                        return_code=0, stdout=f"{digest}  target\n", stderr=""
                    )
                return await super().exec(command=command, user=user, env=env)

        flattened = None
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tests = root / "tests"
            tests.mkdir()
            (tests / "input.csv").write_bytes(content)
            test = tests / "test.sh"
            test.write_text(
                "cp /tests/input.csv .\npytest /tests/score.py\n",
                encoding="utf-8",
            )

            for mode, expected in (
                ("upload", "dependency_fixture_upload"),
                ("stat", "dependency_fixture_stat"),
            ):
                async def invoke_probe():
                    return await readiness._await_tail_stage(
                        lambda: readiness._probe_verifier_container(
                            FailingFixtureEnvironment(mode),
                            {},
                            readiness.EnvironmentPaths(),
                            tests,
                            test,
                            False,
                            900.0,
                        ),
                        stage="dependency setup probe",
                        timeout_seconds=1.0,
                        tail_deadline=asyncio.get_running_loop().time() + 1.0,
                    )

                with self.subTest(mode=mode), self.assertRaises(
                    readiness.ReadinessStageError
                ) as raised:
                    asyncio.run(invoke_probe())
                self.assertEqual(raised.exception.subcategory, expected)
                flattened = readiness._flatten_task_probe_error(
                    task_index=0,
                    task_name="fixture-task",
                    error=raised.exception,
                )
                self.assertEqual(flattened.subcategory, expected)
                self.assertNotIn(sentinel, str(flattened))

            assert flattened is not None
            config = root / "config.json"
            config.write_text("{}", encoding="utf-8")
            stderr = io.StringIO()
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "verifier_readiness.py",
                        "--config",
                        str(config),
                        "--ledger",
                        str(root / "ledger.json"),
                        "--domain-state",
                        str(root),
                    ],
                ),
                mock.patch.object(
                    readiness,
                    "run",
                    new=mock.AsyncMock(side_effect=flattened),
                ),
                contextlib.redirect_stderr(stderr),
            ):
                return_code = readiness.main()
        self.assertEqual(return_code, 78)
        self.assertIn("subcategory=dependency_fixture_stat", stderr.getvalue())
        self.assertNotIn(sentinel, stderr.getvalue())

    def test_concurrent_failure_awaits_every_probe_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(6):
                path = root / f"task-{index}"
                path.mkdir()
                paths.append(path)
            projection = {key: "" for key in readiness.PROJECTION_KEYS}
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "verifier": {"env": projection},
                        "tasks": [{"path": str(path)} for path in paths],
                    }
                ),
                encoding="utf-8",
            )
            ledger = root / "ledger.json"
            started: list[str] = []
            cleaned: list[str] = []
            active = 0
            max_active = 0
            first_wave_ready = asyncio.Event()
            lowest_failure = readiness.ReadinessError("lowest-index failure")

            async def probe(path, _projection, _session, _state_dir, _image_semaphore):
                nonlocal active, max_active
                started.append(path.name)
                active += 1
                max_active = max(max_active, active)
                if len(started) == 3:
                    first_wave_ready.set()
                try:
                    await first_wave_ready.wait()
                    if path.name == "task-2":
                        raise readiness.ReadinessError("higher-index failure")
                    if path.name == "task-0":
                        await asyncio.sleep(0.005)
                        raise lowest_failure
                    await asyncio.sleep(0.01)
                    return {"task": path.name}
                finally:
                    active -= 1
                    cleaned.append(path.name)

            with (
                mock.patch.object(readiness, "probe_task_readiness", side_effect=probe),
                mock.patch.object(
                    readiness,
                    "_probe_primary_registry_transport",
                    new=mock.AsyncMock(),
                ),
                self.assertRaises(readiness.TaskProbeStageError) as raised,
            ):
                asyncio.run(readiness.run(config, ledger, root, max_concurrency=3))
            self.assertEqual(
                (
                    raised.exception.task_index,
                    raised.exception.task_name,
                    raised.exception.stage,
                    raised.exception.kind,
                    raised.exception.category,
                ),
                (0, "task-0", "task probe", "exception", "internal"),
            )
            self.assertNotIn("lowest-index failure", str(raised.exception))
            self.assertIs(raised.exception.__cause__, lowest_failure)
            self.assertEqual(started, [path.name for path in paths[:3]])
            self.assertLessEqual(len(started), 3)
            self.assertEqual(max_active, 3)
            self.assertCountEqual(cleaned, started)
            self.assertFalse(ledger.exists())

    def test_no_setup_requires_a_statically_known_scorer(self):
        plan = readiness.build_dependency_setup_plan(
            "#!/bin/sh\npytest -q /tests/test_outputs.py\n", Path("test.sh")
        )
        self.assertEqual(plan.mode, "no_setup")
        self.assertEqual(plan.steps, ())
        with self.assertRaisesRegex(readiness.ReadinessError, "scoring boundary"):
            readiness.build_dependency_setup_plan(
                "#!/bin/sh\ncustom-test-runner /tests/test_outputs.py\n",
                Path("test.sh"),
            )

    def test_dynamic_or_ambiguous_setup_fails_closed(self):
        for script in (
            "cat <<'EOF'\napt-get update\nEOF\npytest -q\n",
            "if command -v uv; then\n  uv sync\nfi\npytest -q\n",
            "bash -c 'apt-get install -y curl'\npytest -q\n",
            "pytest -q\npip install late-package\n",
            "curl $DYNAMIC_URL | sh\npytest -q\n",
            "export READY=1; pytest /tests/scoring-sentinel.py\n",
            "mkdir -p /tmp/ready && pytest /tests/scoring-sentinel.py\n",
            "apt-get update; pytest /tests/scoring-sentinel.py\n",
            "apt-get update > /tmp/index; pytest /tests/scoring-sentinel.py\n",
            "apt-get update && pytest /tests/scoring-sentinel.py\n",
        ):
            with self.subTest(script=script):
                with self.assertRaises(readiness.ReadinessError):
                    readiness.build_dependency_setup_plan(script, Path("test.sh"))

    def test_native_build_chain_is_admitted_without_the_scorer(self):
        script = """#!/bin/bash
make clean && ./configure && make -j4
pytest /tests/scoring-sentinel.py
"""
        plan = readiness.build_dependency_setup_plan(script, Path("test.sh"))
        self.assertEqual(
            [(step.kind, step.command) for step in plan.steps],
            [
                ("compound_setup", "make clean && ./configure && make -j4"),
                ("minimal_entrypoint", "pytest --version"),
            ],
        )
        rendered = readiness._render_dependency_setup_command(plan)
        self.assertIn("make clean && ./configure && make -j4", rendered)
        self.assertNotIn("scoring-sentinel", rendered)

    def test_native_build_chain_rejects_arbitrary_make_target(self):
        with self.assertRaises(readiness.ReadinessError):
            readiness.build_dependency_setup_plan(
                "make test && pytest /tests/scoring-sentinel.py\n",
                Path("test.sh"),
            )

    def test_static_native_setup_prefix_supports_clone_and_build_capture(self):
        script = """#!/bin/bash
git clone https://example.invalid/project.git original && cd original && git checkout release-1 && cd ..
rm -rf project/tests && cp -r original/tests project/
cd project
make clean && ./configure && make -j4
rm output.txt
make -C tests one DIR=tests/basic | tee output.txt
pytest /tests/scoring-sentinel.py
"""
        plan = readiness.build_dependency_setup_plan(script, Path("test.sh"))
        rendered = readiness._render_dependency_setup_command(plan)
        self.assertIn("git clone https://example.invalid/project.git", rendered)
        self.assertIn("make -C tests one DIR=tests/basic | tee output.txt", rendered)
        self.assertNotIn("scoring-sentinel", rendered)

    def test_native_build_prefix_preserves_non_strict_verifier_semantics(self):
        plan = readiness.build_dependency_setup_plan(
            "make -C tests one | tee output.txt\npytest /tests/score.py\n",
            Path("test.sh"),
        )
        rendered = readiness._render_dependency_setup_command(plan)
        self.assertIn("set +e", rendered)
        self.assertIn(readiness.STEP_EXIT_STATUS_PREFIX + "0=%s", rendered)
        self.assertIn("set -e", rendered)
        self.assertNotIn("/tests/score.py", rendered)

    def test_filesystem_normalization_preserves_non_strict_verifier_semantics(self):
        plan = readiness.build_dependency_setup_plan(
            "rm optional-output.txt\npytest /tests/score.py\n",
            Path("test.sh"),
        )
        rendered = readiness._render_dependency_setup_command(plan)
        self.assertIn("set +e\nrm optional-output.txt", rendered)
        self.assertNotIn("/tests/score.py", rendered)

    def test_static_verifier_data_helper_is_setup_but_test_script_is_not(self):
        plan = readiness.build_dependency_setup_plan(
            "python3 /tests/gen_large_csv.py input\npytest /tests/score.py\n",
            Path("test.sh"),
        )
        self.assertEqual(plan.steps[0].kind, "helper_setup")
        rendered = readiness._render_dependency_setup_command(plan)
        self.assertIn("python3 /tests/gen_large_csv.py input", rendered)
        self.assertNotIn("/tests/score.py", rendered)
        self.assertIsNone(
            readiness._classify_setup_command("python3 /tests/test_outputs.py")
        )


if __name__ == "__main__":
    unittest.main()
