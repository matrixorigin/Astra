"""Harbor adapter that evaluates the real Astra CLI inside task containers.

The adapter intentionally contains no benchmark-specific prompting or task
logic. Harbor owns environment construction and verification; Astra owns the
agent loop, tool execution, permissions, and durable session trace. The CLI
artifact crosses arbitrary task-image libc boundaries, so callers should pass
the portable musl release binary produced by Astra's release build.
"""

import json
import hashlib
import os
import re
import shlex
import tempfile
import tomllib
from pathlib import Path, PurePosixPath
from typing import override

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths
from harbor_adapter_env import (
    astra_chat_command,
    astra_inner_timeout,
    astra_runtime_env,
)


_GIT_SHA_RE = re.compile(r"^[0-9a-fA-F]{40}$")
_SHA256_RE = re.compile(r"^[0-9a-fA-F]{64}$")
_ASTRA_PARTIAL_EXIT_CODE = 5
_BENCHMARK_TARGET = "x86_64-unknown-linux-musl"
_BENCHMARK_PROFILE = "debug"
_HARBOR_LOCK_SCHEMA = 2


def trial_official_agent_timeout(logs_dir: Path) -> int:
    """Read this trial's sealed official deadline, never a job-global value."""
    lock_path = logs_dir.parent / "lock.json"
    try:
        lock = json.loads(lock_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError("Harbor trial lock is unavailable") from error
    if not isinstance(lock, dict) or lock.get("schema_version") != _HARBOR_LOCK_SCHEMA:
        raise RuntimeError("unsupported Harbor trial lock schema")
    if lock.get("timeout_multiplier") != 1.0:
        raise RuntimeError("scored trials require timeout_multiplier=1.0")
    if lock.get("agent_timeout_multiplier") is not None:
        raise RuntimeError("scored trials forbid agent timeout multipliers")
    agent = lock.get("agent")
    if not isinstance(agent, dict) or agent.get("kwargs") not in ({}, None):
        raise RuntimeError("scored trials forbid agent timeout overrides")
    if agent.get("override_timeout_sec") is not None or agent.get("max_timeout_sec") is not None:
        raise RuntimeError("scored trials forbid agent deadline overrides")
    task = lock.get("task")
    task_path = task.get("path") if isinstance(task, dict) else None
    if not isinstance(task_path, str) or not task_path:
        raise RuntimeError("Harbor trial lock lacks sealed task path")
    task_root = Path(task_path)
    if task_root.is_symlink() or not task_root.is_dir():
        raise RuntimeError("sealed task root must be a real directory")
    path = task_root / "task.toml"
    if path.is_symlink():
        raise RuntimeError("sealed task manifest must not be a symlink")
    try:
        timeout = tomllib.loads(path.read_text(encoding="utf-8"))["agent"]["timeout_sec"]
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        raise RuntimeError("sealed task manifest lacks agent timeout") from error
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or int(timeout) != timeout or timeout <= 0:
        raise RuntimeError("sealed task agent timeout must be a positive integer")
    return int(timeout)


def scoreable_interrupted_outcome(
    stdout: str | None, return_code: int
) -> dict[str, object] | None:
    """Recognize a typed Astra partial result without hiding process failures.

    Astra uses a non-zero process status for resumable, incomplete execution.
    Harbor should still run the task verifier for that domain outcome.  Every
    field below is required so a crash, timeout, stale output, or malformed
    envelope remains an infrastructure/agent exception.
    """

    if (
        type(return_code) is not int
        or return_code != _ASTRA_PARTIAL_EXIT_CODE
        or not stdout
    ):
        return None
    try:
        outcome = json.loads(stdout)
    except (TypeError, json.JSONDecodeError):
        return None
    if not isinstance(outcome, dict):
        return None
    interruption_kind = outcome.get("interruption_kind")
    if (
        type(outcome.get("exit_code")) is not int
        or outcome.get("exit_code") != _ASTRA_PARTIAL_EXIT_CODE
        or outcome.get("final_state") != "interrupted"
        or outcome.get("completion_disposition") != "interrupted"
        or outcome.get("success") is not False
        or outcome.get("error_kind") != "partial"
        or not isinstance(interruption_kind, str)
        or not interruption_kind.strip()
    ):
        return None
    return outcome


def validate_benchmark_provenance(binary: Path, get_env) -> tuple[str, str]:
    """Require an auditable source revision and exact uploaded binary hash.

    Harbor results are otherwise easy to misread after a rebuild: a job name
    can mention one revision while the adapter silently uploads another
    artifact.  This check is deliberately provider/task agnostic and runs
    before the binary enters a task container.
    """

    expected_git_sha = (get_env("ASTRA_EXPECTED_BUILD_GIT_SHA") or "").strip()
    expected_binary_sha = (get_env("ASTRA_HARNESS_BINARY_SHA256") or "").strip()
    if not _GIT_SHA_RE.fullmatch(expected_git_sha):
        raise ValueError(
            "ASTRA_EXPECTED_BUILD_GIT_SHA must be a 40-hex source revision "
            "for benchmark runs"
        )
    if not _SHA256_RE.fullmatch(expected_binary_sha):
        raise ValueError(
            "ASTRA_HARNESS_BINARY_SHA256 must be a 64-hex artifact digest "
            "for benchmark runs"
        )

    digest = hashlib.sha256()
    with binary.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    actual_binary_sha = digest.hexdigest()
    if actual_binary_sha.lower() != expected_binary_sha.lower():
        raise RuntimeError(
            "Astra binary provenance mismatch: expected the configured "
            "artifact digest, but the selected file has a different SHA-256"
        )
    return expected_git_sha.lower(), actual_binary_sha


def validate_embedded_build_info(raw: str, expected_git_sha: str) -> dict[str, object]:
    """Validate the binary that actually arrived in the task container."""
    try:
        value = json.loads(raw)
    except (TypeError, json.JSONDecodeError) as error:
        raise RuntimeError(
            "Uploaded Astra binary returned invalid build info"
        ) from error
    expected_keys = {"schema", "git_sha", "git_dirty", "target", "profile"}
    expected = {
        "schema": "astra.build_info.v1",
        "git_sha": expected_git_sha,
        "git_dirty": False,
        "target": _BENCHMARK_TARGET,
        "profile": _BENCHMARK_PROFILE,
    }
    if not isinstance(value, dict) or set(value) != expected_keys or value != expected:
        raise RuntimeError(
            "Uploaded Astra binary build identity does not match the sealed benchmark artifact"
        )
    return value


def validate_stream_event_jsonl(path: Path) -> int:
    """Validate the isolated machine-event artifact and exact tool closure."""
    active_tools: set[str] = set()
    event_count = 0
    try:
        with path.open("r", encoding="utf-8") as handle:
            for line_number, raw_line in enumerate(handle, 1):
                if not raw_line.endswith("\n"):
                    raise RuntimeError(
                        f"Astra machine event {line_number} is not newline terminated"
                    )
                try:
                    event = json.loads(raw_line)
                except json.JSONDecodeError as error:
                    raise RuntimeError(
                        f"Astra machine event {line_number} is invalid JSON"
                    ) from error
                if not isinstance(event, dict) or not isinstance(event.get("type"), str):
                    raise RuntimeError(
                        f"Astra machine event {line_number} lacks a typed JSON object"
                    )
                event_type = event["type"]
                if event_type in {"tool_started", "tool_completed"}:
                    tool_id = event.get("tool_use_id")
                    if not isinstance(tool_id, str) or not tool_id:
                        raise RuntimeError(
                            f"Astra machine event {line_number} lacks a tool identity"
                        )
                    if event_type == "tool_started":
                        if tool_id in active_tools:
                            raise RuntimeError(
                                f"Astra machine event {line_number} repeats an active tool"
                            )
                        active_tools.add(tool_id)
                    elif tool_id not in active_tools:
                        raise RuntimeError(
                            f"Astra machine event {line_number} completes an unknown tool"
                        )
                    else:
                        active_tools.remove(tool_id)
                event_count += 1
    except (OSError, UnicodeDecodeError) as error:
        raise RuntimeError("Astra machine-event artifact is unavailable") from error
    if event_count == 0:
        raise RuntimeError("Astra machine-event artifact is empty")
    if active_tools:
        raise RuntimeError("Astra machine-event artifact has unresolved tool executions")
    return event_count


class Astra(BaseInstalledAgent):
    """Run a host-built Astra CLI against Harbor's isolated task workspace."""

    _REMOTE_BINARY = PurePosixPath("/usr/local/bin/astra")
    _REMOTE_ACCESS_TOKEN_FILE = PurePosixPath("/run/astra-access-token")
    _OUTPUT_FILE = "astra-output.json"

    @staticmethod
    @override
    def name() -> str:
        return "astra"

    @property
    @override
    def extra_env(self) -> dict[str, str]:
        """Expose only non-secret agent variables to Harbor's exec overlay.

        Harbor applies ``agent.extra_env`` to every container exec call.  A
        token placed there is serialized by the Docker backend as ``-e`` and
        can therefore appear in host process arguments.  ``install`` still
        resolves the token through ``_get_env`` (which reads the private
        adapter configuration/host environment), but the scoped overlay must
        never contain the value.
        """
        environment = super().extra_env
        environment.pop("ASTRA_ACCESS_TOKEN", None)
        environment.pop("ASTRA_ACCESS_TOKEN_FILE", None)
        return environment

    @override
    def get_version_command(self) -> str | None:
        return "astra --version"

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        binary_value = self._get_env("ASTRA_HARBOR_BIN")
        if not binary_value:
            raise ValueError(
                "ASTRA_HARBOR_BIN must point to a portable Astra CLI binary "
                "(use the x86_64-unknown-linux-musl debug artifact)"
            )
        binary = Path(os.path.abspath(Path(binary_value).expanduser()))
        if not binary.is_file():
            raise FileNotFoundError(f"Astra CLI binary not found: {binary}")
        expected_git_sha, _ = validate_benchmark_provenance(binary, self._get_env)

        configured_token = self._get_env("ASTRA_ACCESS_TOKEN")
        if configured_token == "${ASTRA_ACCESS_TOKEN}":
            configured_token = os.environ.get("ASTRA_ACCESS_TOKEN")
        if not configured_token:
            raise ValueError(
                "ASTRA_ACCESS_TOKEN must be available to provision the private "
                "container token file"
            )

        await environment.upload_file(binary, self._REMOTE_BINARY.as_posix())
        await self.exec_as_root(
            environment,
            command=f"chmod 0755 {shlex.quote(self._REMOTE_BINARY.as_posix())}",
        )
        build_info_result = await self.exec_as_agent(
            environment,
            command=f"{shlex.quote(self._REMOTE_BINARY.as_posix())} --build-info-json",
        )
        if build_info_result.return_code != 0:
            raise RuntimeError("Uploaded Astra binary build-info probe failed")
        validate_embedded_build_info(build_info_result.stdout or "", expected_git_sha)

        # mkstemp creates the file with O_EXCL and mode 0600 before any token
        # bytes are written.  Keep it outside Harbor's logs/artifact tree and
        # remove it on every upload path; neither the path nor value is logged.
        token_fd, token_name = tempfile.mkstemp(prefix=".astra-access-token-")
        local_token_file = Path(token_name)
        try:
            with os.fdopen(token_fd, "w", encoding="utf-8") as token_handle:
                token_fd = -1
                token_handle.write(configured_token)
            await environment.upload_file(
                local_token_file, self._REMOTE_ACCESS_TOKEN_FILE.as_posix()
            )
            if environment.default_user is not None:
                owner = str(environment.default_user)
            else:
                # A task may omit an explicit user while its image declares a
                # non-root USER.  Resolve the actual execution identity from
                # the same agent context that will run `astra`; never guess
                # from the host or leave the upload root-owned.
                identity = await self.exec_as_agent(environment, command="id -u")
                raw_uid = (identity.stdout or "").strip()
                try:
                    uid = int(raw_uid, 10)
                except (TypeError, ValueError) as error:
                    raise RuntimeError(
                        "The task image did not return a numeric agent UID"
                    ) from error
                if uid < 0:
                    raise RuntimeError("The task image returned an invalid agent UID")
                owner = str(uid)
            await self.exec_as_root(
                environment,
                command=(
                    f"chown {shlex.quote(owner)} "
                    f"{shlex.quote(self._REMOTE_ACCESS_TOKEN_FILE.as_posix())}"
                ),
            )
            await self.exec_as_root(
                environment,
                command=(
                    f"chmod 0400 {shlex.quote(self._REMOTE_ACCESS_TOKEN_FILE.as_posix())}"
                ),
            )
        finally:
            if token_fd != -1:
                os.close(token_fd)
            local_token_file.unlink(missing_ok=True)
        try:
            await self.exec_as_agent(environment, command="astra --version")
        except Exception as error:
            raise RuntimeError(
                "The uploaded Astra CLI cannot execute in this task image. "
                "Use `cargo build --target x86_64-unknown-linux-musl "
                "--bin astra --features release-vendored-openssl` and pass the "
                "result through ASTRA_HARBOR_BIN."
            ) from error

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name:
            raise ValueError("Astra Harbor runs require a model name")

        output_path = EnvironmentPaths.agent_dir / self._OUTPUT_FILE
        inner_timeout = astra_inner_timeout(
            self._get_env, outer_timeout=trial_official_agent_timeout(self.logs_dir)
        )
        command = astra_chat_command(
            self.model_name,
            instruction,
            output_path.as_posix(),
            timeout_sec=inner_timeout,
        )
        runtime_env = astra_runtime_env(self._get_env)
        # The path is an adapter contract, not user-configurable state.  This
        # keeps an old/malformed benchmark config from pointing the CLI at a
        # host-visible or stale credential file.
        runtime_env["ASTRA_ACCESS_TOKEN_FILE"] = (
            self._REMOTE_ACCESS_TOKEN_FILE.as_posix()
        )
        result = await environment.exec(
            # Match BaseInstalledAgent._exec: the command ends in `tee`, so
            # pipefail is required to preserve Astra's typed non-zero status.
            command=f"set -o pipefail; {command}",
            user=environment.default_user,
            env=runtime_env,
        )
        if result.return_code == 0:
            return
        if scoreable_interrupted_outcome(result.stdout, result.return_code) is not None:
            self.logger.info(
                "Astra returned a typed interrupted outcome; preserving the "
                "trial for task verification",
                extra={"return_code": result.return_code},
            )
            return
        raise self._classify_exec_error(command, result)

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        """Project Astra's typed terminal metrics into Harbor's agent result.

        Harbor cannot infer usage from the human-facing trial log.  The CLI
        writes one strict JSON envelope for every run; keeping this projection
        at the adapter boundary makes benchmark cost/efficiency comparisons
        auditable without changing task prompts or provider behavior.
        """
        output_path = self.logs_dir / self._OUTPUT_FILE
        # Diagnostics remain on stderr and are never parsed as lifecycle
        # evidence. A malformed or polluted dedicated event file invalidates
        # the adapter result instead of being silently ignored.
        validate_stream_event_jsonl(self.logs_dir / f"{self._OUTPUT_FILE}.events")
        try:
            with output_path.open("r", encoding="utf-8") as handle:
                outcome = json.load(handle)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            self.logger.warning("Astra output envelope unavailable: %s", error)
            return

        def non_negative_int(value: object) -> int | None:
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                return None
            return value

        prompt_tokens = non_negative_int(outcome.get("prompt_tokens"))
        completion_tokens = non_negative_int(outcome.get("completion_tokens"))
        cache = outcome.get("cache")
        cache_read = (
            non_negative_int(cache.get("read_tokens"))
            if isinstance(cache, dict)
            else None
        )

        if prompt_tokens is not None:
            context.n_input_tokens = prompt_tokens
        if cache_read is not None:
            context.n_cache_tokens = cache_read
        if completion_tokens is not None:
            context.n_output_tokens = completion_tokens
        cost = outcome.get("cost_usd")
        if isinstance(cost, (int, float)) and not isinstance(cost, bool) and cost >= 0:
            context.cost_usd = float(cost)

        metadata = dict(context.metadata or {})
        metadata["astra"] = {
            key: outcome.get(key)
            for key in (
                "run_id",
                "session_id",
                "final_state",
                "completion_disposition",
                "interruption_kind",
                "server_terminal_unverified",
                "server_terminal_authoritative",
                "llm_rounds",
                "tool_calls_count",
                "tool_record_coverage",
                "token_usage_coverage",
                "success",
                "error_kind",
            )
            if key in outcome
        }
        # These values are injected by the harness launcher after it has
        # verified the server build and selected the portable CLI artifact.
        # Keep them as provenance, never as a substitute for the typed
        # envelope fields above.
        expected_sha = self._get_env("ASTRA_EXPECTED_BUILD_GIT_SHA")
        binary_sha256 = self._get_env("ASTRA_HARNESS_BINARY_SHA256")
        if expected_sha:
            metadata["astra"]["expected_build_git_sha"] = expected_sha
        if binary_sha256:
            metadata["astra"]["binary_sha256"] = binary_sha256
        context.metadata = metadata
