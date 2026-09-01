import hashlib
import json
import os
import shlex
import sys
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from harbor_adapter_env import (
    DEFAULT_HARBOR_AGENT_TIMEOUT_SEC,
    DEFAULT_KILL_AFTER_SEC,
    DEFAULT_PROCESS_CUSHION_SEC,
    astra_chat_command,
    astra_inner_timeout,
    astra_runtime_env,
)
from harbor_adapter import (
    Astra,
    scoreable_interrupted_outcome,
    trial_official_agent_timeout,
    validate_benchmark_provenance,
    validate_embedded_build_info,
    validate_stream_event_jsonl,
)
from harbor.models.agent.context import AgentContext
from harbor.agents.installed.base import NonZeroAgentExitCodeError
from harbor.environments.base import ExecResult


def embedded_build_info() -> str:
    return json.dumps(
        {
            "schema": "astra.build_info.v1",
            "git_sha": "a" * 40,
            "git_dirty": False,
            "target": "x86_64-unknown-linux-musl",
            "profile": "debug",
        }
    )


class AstraRuntimeEnvTests(unittest.TestCase):
    def test_machine_events_are_strict_jsonl_with_closed_tool_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            path.write_text(
                '\n'.join(
                    (
                        '{"type":"session_bound","session_id":"s"}',
                        '{"type":"tool_started","tool_use_id":"call-1"}',
                        '{"type":"tool_completed","tool_use_id":"call-1"}',
                    )
                )
                + "\n",
                encoding="utf-8",
            )
            self.assertEqual(validate_stream_event_jsonl(path), 3)

            path.write_text(
                '{"type":"tool_started","tool_use_id":"call-1"}\n'
                "permissive mode warning: command allowed\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(RuntimeError, "invalid JSON"):
                validate_stream_event_jsonl(path)

    def test_uploaded_build_info_is_exact_and_closed(self):
        value = validate_embedded_build_info(embedded_build_info(), "a" * 40)
        self.assertEqual(value["profile"], "debug")
        for mutation in (
            {"git_sha": "b" * 40},
            {"git_dirty": True},
            {"target": "x86_64-unknown-linux-gnu"},
            {"profile": "release"},
            {"extra": "spoof"},
        ):
            payload = json.loads(embedded_build_info())
            payload.update(mutation)
            with self.assertRaises(RuntimeError):
                validate_embedded_build_info(json.dumps(payload), "a" * 40)

    def test_typed_interruption_is_scoreable(self):
        outcome = {
            "exit_code": 5,
            "final_state": "interrupted",
            "completion_disposition": "interrupted",
            "interruption_kind": "execution_incomplete",
            "success": False,
            "error_kind": "partial",
        }

        self.assertEqual(scoreable_interrupted_outcome(json.dumps(outcome), 5), outcome)

    def test_nonzero_without_matching_typed_interruption_remains_an_exception(self):
        valid = {
            "exit_code": 5,
            "final_state": "interrupted",
            "completion_disposition": "interrupted",
            "interruption_kind": "execution_incomplete",
            "success": False,
            "error_kind": "partial",
        }
        invalid_outcomes = (
            None,
            "not-json",
            json.dumps({**valid, "exit_code": 4}),
            json.dumps({**valid, "final_state": "completed"}),
            json.dumps({**valid, "completion_disposition": "completed"}),
            json.dumps({**valid, "interruption_kind": None}),
            json.dumps({**valid, "success": True}),
            json.dumps({**valid, "error_kind": "internal"}),
            json.dumps({**valid, "exit_code": 5.0}),
            json.dumps({**valid, "exit_code": True}),
        )

        for stdout in invalid_outcomes:
            with self.subTest(stdout=stdout):
                self.assertIsNone(scoreable_interrupted_outcome(stdout, 5))
        self.assertIsNone(scoreable_interrupted_outcome(json.dumps(valid), 0))
        for status in (124, 137, 143):
            with self.subTest(status=status):
                self.assertIsNone(
                    scoreable_interrupted_outcome(
                        json.dumps({**valid, "exit_code": status}), status
                    )
                )

    def test_benchmark_provenance_requires_matching_source_and_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "astra"
            binary.write_bytes(b"fresh-binary")
            digest = __import__("hashlib").sha256(binary.read_bytes()).hexdigest()
            values = {
                "ASTRA_EXPECTED_BUILD_GIT_SHA": "a" * 40,
                "ASTRA_HARNESS_BINARY_SHA256": digest,
            }
            source_sha, actual_sha = validate_benchmark_provenance(binary, values.get)
            self.assertEqual(source_sha, "a" * 40)
            self.assertEqual(actual_sha, digest)

            values["ASTRA_HARNESS_BINARY_SHA256"] = "b" * 64
            with self.assertRaises(RuntimeError):
                validate_benchmark_provenance(binary, values.get)

    def test_benchmark_provenance_rejects_missing_or_malformed_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "astra"
            binary.write_bytes(b"fresh-binary")
            for values in (
                {},
                {
                    "ASTRA_EXPECTED_BUILD_GIT_SHA": "head-sha",
                    "ASTRA_HARNESS_BINARY_SHA256": "binary-sha",
                },
            ):
                with self.subTest(values=values):
                    with self.assertRaises(ValueError):
                        validate_benchmark_provenance(binary, values.get)

    def test_projects_typed_cli_metrics_into_harbor_context(self):
        with tempfile.TemporaryDirectory() as directory:
            output_path = Path(directory) / "astra-output.json"
            output_path.write_text(
                json.dumps(
                    {
                        "run_id": "run-1",
                        "session_id": "session-1",
                        "prompt_tokens": 1200,
                        "completion_tokens": 340,
                        "cache": {"read_tokens": 900, "creation_tokens": 0},
                        "cost_usd": 0.12,
                        "final_state": "completed",
                        "completion_disposition": "responded_verified",
                        "server_terminal_authoritative": True,
                        "llm_rounds": 4,
                        "tool_calls_count": 3,
                        "tool_record_coverage": "complete",
                        "success": True,
                    }
                )
            )
            Path(f"{output_path}.events").write_text(
                '{"type":"session_bound","session_id":"session-1"}\n',
                encoding="utf-8",
            )

            context = AgentContext()
            with patch.dict(
                os.environ,
                {
                    "ASTRA_EXPECTED_BUILD_GIT_SHA": "head-sha",
                    "ASTRA_HARNESS_BINARY_SHA256": "binary-sha",
                },
            ):
                Astra(
                    Path(directory), model_name="deepseek-v4-flash"
                ).populate_context_post_run(context)

            self.assertEqual(context.n_input_tokens, 1200)
            self.assertEqual(context.n_cache_tokens, 900)
            self.assertEqual(context.n_output_tokens, 340)
            self.assertEqual(context.cost_usd, 0.12)
            self.assertEqual(context.metadata["astra"]["tool_calls_count"], 3)
            self.assertIs(
                context.metadata["astra"]["server_terminal_authoritative"], True
            )
            self.assertEqual(
                context.metadata["astra"]["expected_build_git_sha"], "head-sha"
            )
            self.assertEqual(context.metadata["astra"]["binary_sha256"], "binary-sha")

    def test_missing_or_malformed_cli_metrics_do_not_invent_usage(self):
        with tempfile.TemporaryDirectory() as directory:
            output_path = Path(directory) / "astra-output.json"
            output_path.write_text("not-json")
            Path(f"{output_path}.events").write_text(
                '{"type":"session_bound","session_id":"session-1"}\n',
                encoding="utf-8",
            )
            context = AgentContext()

            Astra(
                Path(directory), model_name="deepseek-v4-flash"
            ).populate_context_post_run(context)

            self.assertTrue(context.is_empty())

    def test_forwards_credentials_and_derives_proxy_bypass_without_exposing_other_values(
        self,
    ):
        configured = {
            "ASTRA_API_URL": "http://172.17.0.1:17003",
            "ASTRA_ACCESS_TOKEN": "secret-token",
            "ASTRA_ACCESS_TOKEN_FILE": "/run/astra-access-token",
            "UNRELATED_SECRET": "must-not-cross-boundary",
        }

        runtime_env = astra_runtime_env(configured.get)

        self.assertEqual(runtime_env["ASTRA_API_URL"], configured["ASTRA_API_URL"])
        self.assertEqual(
            runtime_env["ASTRA_ACCESS_TOKEN_FILE"], "/run/astra-access-token"
        )
        self.assertNotIn("ASTRA_ACCESS_TOKEN", runtime_env)
        self.assertEqual(runtime_env["NO_PROXY"], "localhost,127.0.0.1,::1,172.17.0.1")
        self.assertEqual(runtime_env["no_proxy"], "localhost,127.0.0.1,::1,172.17.0.1")
        self.assertNotIn("UNRELATED_SECRET", runtime_env)

    def test_loopback_api_does_not_duplicate_proxy_bypasses(self):
        runtime_env = astra_runtime_env(
            {
                "ASTRA_API_URL": "http://127.0.0.1:17003",
                "ASTRA_ACCESS_TOKEN": "secret-token",
                "ASTRA_ACCESS_TOKEN_FILE": "/run/astra-access-token",
            }.get
        )

        self.assertEqual(runtime_env["NO_PROXY"], "localhost,127.0.0.1,::1")
        self.assertEqual(runtime_env["no_proxy"], "localhost,127.0.0.1,::1")

    def test_exact_config_placeholders_resolve_from_host_without_shell_expansion(self):
        configured = {
            "ASTRA_API_URL": "${ASTRA_API_URL}",
            "ASTRA_ACCESS_TOKEN": "${ASTRA_ACCESS_TOKEN}",
            "ASTRA_ACCESS_TOKEN_FILE": "/run/astra-access-token",
        }
        with patch.dict(
            "os.environ",
            {
                "ASTRA_API_URL": "http://172.17.0.1:17012",
                "ASTRA_ACCESS_TOKEN": "host-scoped-token",
            },
            clear=False,
        ):
            runtime_env = astra_runtime_env(configured.get)

        self.assertEqual(runtime_env["ASTRA_API_URL"], "http://172.17.0.1:17012")
        self.assertEqual(
            runtime_env["ASTRA_ACCESS_TOKEN_FILE"], "/run/astra-access-token"
        )
        self.assertNotIn("ASTRA_ACCESS_TOKEN", runtime_env)

    def test_non_exact_placeholder_is_not_expanded(self):
        configured = {"ASTRA_ACCESS_TOKEN": "prefix-${ASTRA_ACCESS_TOKEN}"}
        with patch.dict(
            "os.environ", {"ASTRA_ACCESS_TOKEN": "host-token"}, clear=False
        ):
            runtime_env = astra_runtime_env(configured.get)

        self.assertNotIn("ASTRA_ACCESS_TOKEN", runtime_env)

    def test_forwards_only_allowlisted_proxy_variables_in_both_cases(self):
        configured = {
            "ASTRA_API_URL": "http://172.17.0.1:17003",
            "ASTRA_ACCESS_TOKEN": "secret-token",
            "ASTRA_ACCESS_TOKEN_FILE": "/run/astra-access-token",
            "http_proxy": "http://proxy.example:8080",
            "HTTPS_PROXY": "http://secure-proxy.example:8443",
            "all_proxy": "socks5://proxy.example:1080",
            "ASTRA_HARBOR_NO_PROXY": "pypi.org, localhost",
            "UNRELATED_SECRET": "must-not-cross-boundary",
            "FTP_PROXY": "http://unrelated.example:21",
        }

        runtime_env = astra_runtime_env(configured.get)

        self.assertEqual(runtime_env["http_proxy"], configured["http_proxy"])
        self.assertEqual(runtime_env["HTTP_PROXY"], configured["http_proxy"])
        self.assertEqual(runtime_env["https_proxy"], configured["HTTPS_PROXY"])
        self.assertEqual(runtime_env["HTTPS_PROXY"], configured["HTTPS_PROXY"])
        self.assertEqual(runtime_env["all_proxy"], configured["all_proxy"])
        self.assertEqual(runtime_env["ALL_PROXY"], configured["all_proxy"])
        self.assertEqual(
            runtime_env["NO_PROXY"],
            "pypi.org,localhost,127.0.0.1,::1,172.17.0.1",
        )
        self.assertEqual(runtime_env["no_proxy"], runtime_env["NO_PROXY"])
        self.assertNotIn("FTP_PROXY", runtime_env)
        self.assertNotIn("UNRELATED_SECRET", runtime_env)

    def test_ambient_no_proxy_is_not_forwarded_without_opt_in(self):
        runtime_env = astra_runtime_env(
            {
                "ASTRA_API_URL": "http://172.17.0.1:17003",
                "NO_PROXY": "internal.example,10.0.0.0/8",
            }.get
        )

        self.assertEqual(runtime_env["NO_PROXY"], "localhost,127.0.0.1,::1,172.17.0.1")

    def test_rejects_proxy_credentials_and_loopback_proxy(self):
        for value in (
            "http://user:password@proxy.example:8080",
            "http://127.0.0.1:8080",
        ):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    astra_runtime_env(
                        {
                            "ASTRA_API_URL": "http://172.17.0.1:17003",
                            "http_proxy": value,
                        }.get
                    )

    def test_rejects_unbounded_bypass_wildcard(self):
        with self.assertRaises(ValueError):
            astra_runtime_env(
                {
                    "ASTRA_API_URL": "http://172.17.0.1:17003",
                    "ASTRA_HARBOR_NO_PROXY": "*",
                }.get
            )

    def test_omits_absent_optional_values(self):
        self.assertEqual(astra_runtime_env(lambda _name: None), {})

    def test_benchmark_command_uses_container_scoped_bypass_mode(self):
        command = astra_chat_command(
            "deepseek-v4-flash",
            "inspect /etc and don't truncate",
            "/logs/agent/astra-output.json",
        )

        self.assertIn("ASTRA_API_URL is missing", command)
        self.assertIn("Astra access-token file is missing", command)
        self.assertIn('ASTRA_ACCESS_TOKEN="$(cat "$ASTRA_ACCESS_TOKEN_FILE")"', command)
        self.assertNotIn("secret-token", command)
        self.assertIn("astra health >/dev/null", command)
        self.assertIn(
            "timeout --signal=TERM --kill-after=20s 840s astra chat",
            command,
        )
        self.assertIn("--max-wall-time-seconds 825", command)
        self.assertNotIn("--preserve-status", command)
        self.assertIn("--permission-mode bypass", command)
        self.assertIn("--model deepseek-v4-flash", command)
        self.assertIn("'inspect /etc and don'\"'\"'t truncate'", command)
        self.assertTrue(command.endswith("| tee /logs/agent/astra-output.json"))

    def test_benchmark_command_rejects_invalid_lifecycle_limits(self):
        with self.assertRaises(ValueError):
            astra_chat_command(
                "deepseek-v4-flash", "noop", "/logs/out.json", timeout_sec=0
            )

    def test_inner_timeout_leaves_cleanup_margin_under_harbor_deadline(self):
        env = {"ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS": "60"}
        outer = astra_inner_timeout(env.get, outer_timeout=900)
        self.assertEqual(outer, 840)
        command = astra_chat_command(
            "deepseek-v4-flash", "noop", "/logs/out.json", timeout_sec=outer
        )
        self.assertIn(
            f"--max-wall-time-seconds {outer - DEFAULT_PROCESS_CUSHION_SEC}",
            command,
        )
        self.assertLess(
            outer + DEFAULT_KILL_AFTER_SEC,
            DEFAULT_HARBOR_AGENT_TIMEOUT_SEC,
        )
        with self.assertRaises(ValueError):
            astra_inner_timeout(
                {"ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS": "10"}.get,
                outer_timeout=25,
                kill_after_sec=20,
            )
        with self.assertRaises(ValueError):
            astra_inner_timeout(
                {"ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS": "20"}.get,
                outer_timeout=100,
                kill_after_sec=20,
            )
        with self.assertRaises(ValueError):
            astra_chat_command(
                "deepseek-v4-flash", "noop", "/logs/out.json", kill_after_sec=-1
            )

    def test_timeout_status_cannot_masquerade_as_typed_partial(self):
        outcome = json.dumps(
            {
                "exit_code": 5,
                "final_state": "interrupted",
                "completion_disposition": "interrupted",
                "interruption_kind": "execution_incomplete",
                "success": False,
                "error_kind": "partial",
            },
            separators=(",", ":"),
        )
        child = f"trap 'printf %s {shlex.quote(outcome)}; exit 5' TERM; sleep 5"
        result = subprocess.run(
            [
                "/bin/bash",
                "-c",
                "set -o pipefail; timeout --signal=TERM --kill-after=1s "
                f"0.05s /bin/sh -c {shlex.quote(child)} | tee /dev/null",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 124)
        self.assertIsNone(scoreable_interrupted_outcome(result.stdout, 124))

    def test_benchmark_command_rejects_missing_runtime_contract_before_cli(self):
        command = astra_chat_command("deepseek-v4-flash", "noop", "/logs/out.json")

        missing_url = subprocess.run(
            ["/bin/sh", "-c", command],
            env={"PATH": "/usr/bin:/bin"},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(missing_url.returncode, 78)
        self.assertIn("ASTRA_API_URL is missing", missing_url.stderr)

        missing_token = subprocess.run(
            ["/bin/sh", "-c", command],
            env={
                "PATH": "/usr/bin:/bin",
                "ASTRA_API_URL": "http://127.0.0.1:17011",
            },
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(missing_token.returncode, 78)
        self.assertIn("Astra access-token file is missing", missing_token.stderr)


class AstraRunClassificationTests(unittest.IsolatedAsyncioTestCase):
    @staticmethod
    def _write_trial_lock(directory: str, timeout: int = 900, **agent_fields) -> None:
        task = Path(directory) / "task"
        task.mkdir(exist_ok=True)
        (task / "task.toml").write_text(f"[agent]\ntimeout_sec = {timeout}\n")
        (Path(directory) / "lock.json").write_text(json.dumps({
            "schema_version": 2, "timeout_multiplier": 1.0,
            "agent_timeout_multiplier": None,
            "agent": {"kwargs": {}, **agent_fields}, "task": {"path": str(task)},
        }))

    def test_trial_deadline_is_per_trial_and_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self._write_trial_lock(directory, 600)
            self.assertEqual(trial_official_agent_timeout(root / "agent"), 600)
            self._write_trial_lock(directory, 1800)
            self.assertEqual(trial_official_agent_timeout(root / "agent"), 1800)
            self._write_trial_lock(directory, 900, override_timeout_sec=450)
            with self.assertRaises(RuntimeError):
                trial_official_agent_timeout(root / "agent")
            self._write_trial_lock(directory, 900, max_timeout_sec=450)
            with self.assertRaises(RuntimeError):
                trial_official_agent_timeout(root / "agent")
    async def test_install_uploads_token_file_without_putting_secret_in_commands(self):
        class FakeEnvironment:
            default_user = 1000

            def __init__(self):
                self.uploads = []
                self.root_commands = []
                self.source_modes = []
                self.source_paths = []

            async def upload_file(self, source_path, target_path):
                self.source_paths.append((target_path, Path(source_path)))
                self.source_modes.append(Path(source_path).stat().st_mode & 0o777)
                self.uploads.append((target_path, Path(source_path).read_bytes()))

        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "astra"
            binary.write_bytes(b"portable-agent")
            digest = hashlib.sha256(binary.read_bytes()).hexdigest()
            environment = FakeEnvironment()
            agent = Astra(
                Path(directory),
                model_name="deepseek-v4-flash",
                extra_env={
                    "ASTRA_HARBOR_BIN": str(binary),
                    "ASTRA_EXPECTED_BUILD_GIT_SHA": "a" * 40,
                    "ASTRA_HARNESS_BINARY_SHA256": digest,
                    "ASTRA_ACCESS_TOKEN": "secret-token",
                    "ASTRA_ACCESS_TOKEN_FILE": "/malicious/override",
                },
            )
            agent.exec_as_root = AsyncMock(
                side_effect=lambda environment, command: (
                    environment.root_commands.append(command)
                )
            )
            agent.exec_as_agent = AsyncMock(
                return_value=ExecResult(
                    stdout=embedded_build_info(), stderr=None, return_code=0
                )
            )

            await agent.install(environment)

            self.assertIn(
                ("/run/astra-access-token", b"secret-token"), environment.uploads
            )
            self.assertTrue(
                any(
                    target == "/usr/local/bin/astra"
                    for target, _ in environment.uploads
                )
            )
            self.assertTrue(environment.root_commands)
            self.assertIn(0o600, environment.source_modes)
            self.assertTrue(
                any(
                    "chown 1000 /run/astra-access-token" in command
                    for command in environment.root_commands
                )
            )
            self.assertTrue(
                all(
                    "secret-token" not in command
                    for command in environment.root_commands
                )
            )
            self.assertFalse((Path(directory) / ".astra-access-token").exists())
            self.assertTrue(
                all(
                    not source.exists()
                    for target, source in environment.source_paths
                    if target == "/run/astra-access-token"
                )
            )
            self.assertNotIn("ASTRA_ACCESS_TOKEN", agent.extra_env)
            self.assertNotIn("ASTRA_ACCESS_TOKEN_FILE", agent.extra_env)

    async def test_install_cleans_token_source_when_upload_fails(self):
        class FailingEnvironment:
            default_user = 1000

            def __init__(self):
                self.token_source = None

            async def upload_file(self, source_path, target_path):
                if target_path == "/run/astra-access-token":
                    self.token_source = Path(source_path)
                    raise RuntimeError("upload failed")

        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "astra"
            binary.write_bytes(b"portable-agent")
            digest = hashlib.sha256(binary.read_bytes()).hexdigest()
            environment = FailingEnvironment()
            agent = Astra(
                Path(directory),
                model_name="deepseek-v4-flash",
                extra_env={
                    "ASTRA_HARBOR_BIN": str(binary),
                    "ASTRA_EXPECTED_BUILD_GIT_SHA": "a" * 40,
                    "ASTRA_HARNESS_BINARY_SHA256": digest,
                    "ASTRA_ACCESS_TOKEN": "secret-token",
                },
            )
            agent.exec_as_root = AsyncMock()
            agent.exec_as_agent = AsyncMock(
                return_value=ExecResult(
                    stdout=embedded_build_info(), stderr=None, return_code=0
                )
            )

            with self.assertRaises(RuntimeError):
                await agent.install(environment)

            self.assertIsNotNone(environment.token_source)
            self.assertFalse(environment.token_source.exists())

    async def test_install_resolves_image_uid_when_default_user_is_missing(self):
        class ImageUserEnvironment:
            default_user = None

            def __init__(self):
                self.uploads = []
                self.root_commands = []

            async def upload_file(self, source_path, target_path):
                self.uploads.append((target_path, Path(source_path).read_bytes()))

        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "astra"
            binary.write_bytes(b"portable-agent")
            digest = hashlib.sha256(binary.read_bytes()).hexdigest()
            environment = ImageUserEnvironment()
            agent = Astra(
                Path(directory),
                model_name="deepseek-v4-flash",
                extra_env={
                    "ASTRA_HARBOR_BIN": str(binary),
                    "ASTRA_EXPECTED_BUILD_GIT_SHA": "a" * 40,
                    "ASTRA_HARNESS_BINARY_SHA256": digest,
                    "ASTRA_ACCESS_TOKEN": "secret-token",
                },
            )
            agent.exec_as_root = AsyncMock(
                side_effect=lambda environment, command: (
                    environment.root_commands.append(command)
                )
            )
            agent.exec_as_agent = AsyncMock(
                side_effect=[
                    ExecResult(
                        stdout=embedded_build_info(), stderr=None, return_code=0
                    ),
                    ExecResult(stdout="1001\n", stderr=None, return_code=0),
                    ExecResult(stdout="astra 0.1.0\n", stderr=None, return_code=0),
                ]
            )

            await agent.install(environment)

            self.assertTrue(
                any(
                    "chown 1001 /run/astra-access-token" in command
                    for command in environment.root_commands
                )
            )

    async def test_run_preserves_typed_interruption_for_verification(self):
        outcome = {
            "exit_code": 5,
            "final_state": "interrupted",
            "completion_disposition": "interrupted",
            "interruption_kind": "execution_incomplete",
            "success": False,
            "error_kind": "partial",
        }
        environment = SimpleNamespace(
            default_user=1000,
            exec=AsyncMock(
                return_value=ExecResult(
                    stdout=json.dumps(outcome), stderr=None, return_code=5
                )
            ),
        )

        with tempfile.TemporaryDirectory() as directory:
            agent = Astra(Path(directory) / "agent", model_name="deepseek-v4-flash")
            self._write_trial_lock(directory)
            await agent.run("task", environment, AgentContext())

        call = environment.exec.await_args.kwargs
        self.assertTrue(call["command"].startswith("set -o pipefail; "))
        self.assertEqual(call["user"], 1000)
        self.assertNotIn("cwd", call)

    async def test_run_exec_environment_contains_path_not_token_value(self):
        environment = SimpleNamespace(
            default_user=1000,
            exec=AsyncMock(
                return_value=ExecResult(
                    stdout=json.dumps(
                        {
                            "exit_code": 0,
                            "final_state": "completed",
                            "completion_disposition": "completed",
                            "success": True,
                        }
                    ),
                    stderr=None,
                    return_code=0,
                )
            ),
        )

        with tempfile.TemporaryDirectory() as directory:
            agent = Astra(
                Path(directory),
                model_name="deepseek-v4-flash",
                extra_env={
                    "ASTRA_API_URL": "http://172.17.0.1:17015",
                    "ASTRA_ACCESS_TOKEN": "secret-token",
                },
            )
            self._write_trial_lock(directory)
            await agent.run("task", environment, AgentContext())

        call = environment.exec.await_args.kwargs
        self.assertEqual(
            call["env"]["ASTRA_ACCESS_TOKEN_FILE"], "/run/astra-access-token"
        )
        self.assertNotIn("ASTRA_ACCESS_TOKEN", call["env"])
        self.assertNotIn("secret-token", call["command"])

    async def test_run_keeps_malformed_nonzero_as_harbor_exception(self):
        environment = SimpleNamespace(
            default_user=None,
            exec=AsyncMock(
                return_value=ExecResult(
                    stdout="not-json", stderr="crashed", return_code=5
                )
            ),
        )

        with tempfile.TemporaryDirectory() as directory:
            agent = Astra(Path(directory) / "agent", model_name="deepseek-v4-flash")
            self._write_trial_lock(directory)
            with self.assertRaises(NonZeroAgentExitCodeError):
                await agent.run("task", environment, AgentContext())


if __name__ == "__main__":
    unittest.main()
