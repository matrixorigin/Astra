#!/usr/bin/env python3
"""Prove Harbor verifier infrastructure readiness before any scored model call."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import hashlib
import json
import math
import os
import re
import shlex
import shutil
import stat
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path
from pathlib import PurePosixPath
from typing import NamedTuple
from urllib.parse import urlsplit

from harbor.environments.factory import EnvironmentFactory
from harbor.environments.base import ExecResult
from harbor.models.environment_type import EnvironmentType
from harbor.models.task.task import Task
from harbor.models.task.verifier_mode import (
    resolve_effective_verifier_env_config,
    resolve_task_verifier_mode,
)
from harbor.models.trial.config import AgentConfig as TrialAgentConfig
from harbor.models.trial.config import EnvironmentConfig as TrialEnvironmentConfig
from harbor.models.trial.config import ServiceVolumeConfig
from harbor.models.trial.paths import EnvironmentPaths, TrialPaths
from harbor.trial.network_policy import resolve_trial_network_plan
from harbor.environments.docker.docker import _sanitize_docker_compose_project_name

try:
    from recovery_environment import project_recovery_compose_env
except ModuleNotFoundError:
    from scripts.harness.recovery_environment import project_recovery_compose_env


SCHEMA = "astra.harness.verifier_readiness.v6"
PROJECTION_KEYS = {
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
}
STEP_RECEIPT_PREFIX = "__ASTRA_DEPENDENCY_SETUP_STEP_V1__"
STEP_EXIT_STATUS_PREFIX = "__ASTRA_DEPENDENCY_SETUP_EXIT_V1__"
VENV_DIGEST_PREFIX = "__ASTRA_VENV_ACTIVATE_SHA256_V1__"
DEPENDENCY_SETUP_POLICY = "astra.harness.dependency_setup_entrypoint.v3"
MAX_SOURCE_BYTES = 64 * 1024
MAX_STATIC_BINDING_VALUE_BYTES = 512
DEFAULT_MAX_CONCURRENCY = 4
MAX_CONCURRENCY = 8
IMAGE_MATERIALIZATION_CONCURRENCY = 1
IMAGE_INSPECT_TIMEOUT_SECONDS = 15.0
REGISTRY_TRANSPORT_TIMEOUT_SECONDS = 8.0
MAX_IMAGE_INSPECTIONS_PER_MATERIALIZATION = 3
PROCESS_TERMINATION_GRACE_SECONDS = 2.0
PROCESS_TERMINATION_TIMEOUT_SECONDS = 2 * PROCESS_TERMINATION_GRACE_SECONDS
NETWORK_TRANSITION_TIMEOUT_SECONDS = 60.0
NETWORK_TRANSITIONS_PER_PROBE = 2
TAIL_PROCESS_TERMINATION_SECONDS = PROCESS_TERMINATION_TIMEOUT_SECONDS
CLEANUP_GRACE_SECONDS = 64.0
SAFE_LITERAL_ASSIGNMENTS = {
    "DEBIAN_FRONTEND": {"noninteractive"},
}
RUNNER_MINIMAL_ENTRYPOINTS = {
    "go_test": "go version",
    "cargo_test": "cargo --version",
    "npm_test": "npm list --depth=0",
    "pnpm_test": "pnpm list --depth=0",
    "yarn_test": "yarn list --depth=0",
}


class ReadinessError(RuntimeError):
    pass


READINESS_CONTRACT_SUBCATEGORIES = {
    "plan_static_binding_disallowed",
    "plan_unclassified_pre_scoring_command",
}
DEPENDENCY_PROBE_SUBCATEGORIES = {
    "dependency_readability",
    "dependency_batch_exec",
    "dependency_batch_receipt",
    "dependency_source_resolve",
    "dependency_source_stat",
    "dependency_source_digest",
    "dependency_source_policy",
    "dependency_fixture_workdir",
    "dependency_fixture_source",
    "dependency_fixture_destination",
    "dependency_fixture_upload",
    "dependency_fixture_stat",
    "dependency_fixture_digest",
    "dependency_fixture_content",
}
READINESS_SUBCATEGORIES = (
    READINESS_CONTRACT_SUBCATEGORIES | DEPENDENCY_PROBE_SUBCATEGORIES
)


class ReadinessContractError(ReadinessError):
    def __init__(self, subcategory: str) -> None:
        if subcategory not in READINESS_CONTRACT_SUBCATEGORIES:
            raise ValueError("readiness contract subcategory is invalid")
        self.subcategory = subcategory
        message = {
            "plan_static_binding_disallowed": "static verifier binding is unavailable",
            "plan_unclassified_pre_scoring_command": (
                "cannot prove verifier dependency/scoring boundary"
            ),
        }[subcategory]
        super().__init__(f"{message} [subcategory={subcategory}]")


class DependencyProbeError(ReadinessError):
    """A closed, secret-free dependency-probe boundary failure."""

    def __init__(self, subcategory: str) -> None:
        if subcategory not in DEPENDENCY_PROBE_SUBCATEGORIES:
            raise ValueError("dependency probe subcategory is invalid")
        self.subcategory = subcategory
        super().__init__(
            "verifier dependency probe failed "
            f"[subcategory={subcategory}]"
        )


@contextlib.contextmanager
def _dependency_probe_boundary(subcategory: str):
    try:
        yield
    except (asyncio.CancelledError, DependencyProbeError):
        raise
    except BaseException as error:
        raise DependencyProbeError(subcategory) from error


class ImageMaterializationError(ReadinessError):
    """A fail-closed, stage-addressable Docker image readiness failure."""

    def __init__(self, stage: str, kind: str, detail: str) -> None:
        self.stage = stage
        self.kind = kind
        self.detail = detail or "no diagnostic available"
        super().__init__(
            "verifier image materialization failed "
            f"[stage={stage}, kind={kind}]: {self.detail}"
        )


class ReadinessStageError(ReadinessError):
    """A static, non-secret-bearing failure at a named readiness stage."""

    def __init__(
        self,
        stage: str,
        kind: str,
        category: str,
        *,
        subcategory: str | None = None,
    ) -> None:
        if (
            subcategory is not None
            and subcategory not in READINESS_SUBCATEGORIES
        ):
            raise ValueError("readiness stage subcategory is invalid")
        self.stage = stage
        self.kind = kind
        self.category = category
        self.subcategory = subcategory
        suffix = f", subcategory={subcategory}" if subcategory is not None else ""
        super().__init__(
            "verifier readiness stage failed "
            f"[stage={stage}, kind={kind}, category={category}{suffix}]"
        )


TASK_NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
TASK_PROBE_STAGES = {
    "cache_inspect",
    "cache_reinspect",
    "pull",
    "post_inspect",
    "compose collector",
    "environment start",
    "environment healthcheck",
    "network transition to verifier phase",
    "network transition to baseline",
    "dependency setup probe",
    "cleanup",
    "cleanup prepare_logs",
    "cleanup compose_down",
    "cleanup cleanup_mounts",
    "cleanup cleanup_resources",
    "cleanup cleanup_env",
    "cleanup cleanup_egress",
    "cleanup whole_cleanup",
    "cleanup quiescence",
    "cleanup quiescence containers",
    "cleanup quiescence networks",
    "cleanup quiescence volumes",
    "task probe",
}
TASK_PROBE_KINDS = {
    "timeout",
    "exception",
    "spawn",
    "nonzero_exit",
    "invalid_output",
    "reap_failed",
    "install_failed",
}
TASK_PROBE_CATEGORIES = {
    "deadline_exceeded",
    "process_cleanup",
    "unsupported_environment",
    "image_materialization",
    "readiness_contract",
    "operating_system",
    "invalid_input",
    "runtime",
    "internal",
}


class TaskProbeStageError(ReadinessError):
    """Closed, secret-free task failure suitable for stderr rendering."""

    def __init__(
        self,
        *,
        task_index: int,
        task_name: str,
        stage: str,
        kind: str,
        category: str,
        subcategory: str | None = None,
    ) -> None:
        if isinstance(task_index, bool) or not isinstance(task_index, int):
            raise ValueError("task_index is invalid")
        if task_index < 0:
            raise ValueError("task_index is invalid")
        if TASK_NAME_PATTERN.fullmatch(task_name) is None:
            raise ValueError("task_name is invalid")
        if stage not in TASK_PROBE_STAGES:
            raise ValueError("task probe stage is invalid")
        if kind not in TASK_PROBE_KINDS:
            raise ValueError("task probe kind is invalid")
        if category not in TASK_PROBE_CATEGORIES:
            raise ValueError("task probe category is invalid")
        if (
            subcategory is not None
            and subcategory not in READINESS_SUBCATEGORIES
        ):
            raise ValueError("task probe subcategory is invalid")
        self.task_index = task_index
        self.task_name = task_name
        self.stage = stage
        self.kind = kind
        self.category = category
        self.subcategory = subcategory
        suffix = f", subcategory={subcategory}" if subcategory is not None else ""
        super().__init__(
            "verifier readiness task failed "
            f"[task_index={task_index}, task_name={task_name}, "
            f"stage={stage}, kind={kind}, category={category}{suffix}]"
        )


class OwnedProcessResult(NamedTuple):
    returncode: int
    stdout: str
    stderr: str


class DependencySetupStep(NamedTuple):
    kind: str
    command: str


class StaticBinding(NamedTuple):
    name: str
    value: str
    assignment_sha256: str


class FixtureStage(NamedTuple):
    sequence: int
    step_index: int
    source_relative: str
    basename: str
    source_sha256: str

    def receipt(self) -> dict[str, object]:
        source_path = str(PurePosixPath("/tests") / self.source_relative)
        return {
            "sequence": self.sequence,
            "step_index": self.step_index,
            "source_relative_sha256": hashlib.sha256(
                self.source_relative.encode()
            ).hexdigest(),
            "source_path_sha256": hashlib.sha256(source_path.encode()).hexdigest(),
            "basename_sha256": hashlib.sha256(self.basename.encode()).hexdigest(),
            "source_sha256": self.source_sha256,
        }


class ScoringAdapter(NamedTuple):
    runner_family: str
    minimal_entrypoint: str | None
    resolves_dependencies: bool = False


class EnvironmentDelta(NamedTuple):
    path_prepend: tuple[str, ...] = ()

    def merged(self, other: "EnvironmentDelta") -> "EnvironmentDelta":
        return EnvironmentDelta((*self.path_prepend, *other.path_prepend))

    def receipt_sha256(self) -> str:
        return canonical_json_sha256(
            {
                "path_prepend_sha256": [
                    hashlib.sha256(path.encode()).hexdigest()
                    for path in self.path_prepend
                ],
            }
        )


class DependencySetupPlan(NamedTuple):
    runner_family: str
    steps: tuple[DependencySetupStep, ...]
    scoring_command_sha256: str
    fixtures: tuple[FixtureStage, ...] = ()

    @property
    def mode(self) -> str:
        return "executed" if self.steps or self.fixtures else "no_setup"

    def receipt_plan(self) -> dict[str, object]:
        rendered = (
            _render_dependency_setup_command(self)
            if self.steps
            and not any(
                step.kind in {"environment_source", "fixture_stage"}
                for step in self.steps
            )
            else None
        )
        return {
            "policy": DEPENDENCY_SETUP_POLICY,
            "shell": "bash",
            "runner_family": self.runner_family,
            "rendered_command_sha256": (
                hashlib.sha256(rendered.encode()).hexdigest()
                if rendered is not None
                else None
            ),
            "scoring_command_sha256": self.scoring_command_sha256,
            "fixtures": [fixture.receipt() for fixture in self.fixtures],
            "steps": [
                {
                    "kind": step.kind,
                    "command_sha256": hashlib.sha256(step.command.encode()).hexdigest(),
                }
                for step in self.steps
            ],
        }


def canonical_json_sha256(value: object) -> str:
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def task_tree_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    for entry in sorted(
        path.rglob("*"), key=lambda value: value.relative_to(path).as_posix()
    ):
        relative = entry.relative_to(path).as_posix()
        status = entry.lstat()
        if entry.is_symlink():
            kind = b"symlink"
            content = os.readlink(entry).encode("utf-8", errors="surrogateescape")
        elif entry.is_file():
            kind = b"file"
            content = entry.read_bytes()
        elif entry.is_dir():
            kind = b"directory"
            content = b""
        else:
            raise ReadinessError(f"unsupported task tree entry: {entry}")
        digest.update(
            kind
            + b"\0"
            + relative.encode()
            + b"\0"
            + f"{status.st_mode & 0o7777:o}".encode()
            + b"\0"
            + content
        )
    return digest.hexdigest()


async def _terminate_owned_process(process: asyncio.subprocess.Process) -> None:
    if process.returncode is not None:
        return
    try:
        process.terminate()
    except ProcessLookupError:
        return
    try:
        await asyncio.wait_for(
            process.communicate(), timeout=PROCESS_TERMINATION_GRACE_SECONDS
        )
    except asyncio.TimeoutError:
        try:
            process.kill()
        except ProcessLookupError:
            return
        await asyncio.wait_for(
            process.communicate(), timeout=PROCESS_TERMINATION_GRACE_SECONDS
        )


async def _await_owned_task(
    owner: asyncio.Task, pending_cancellation: asyncio.CancelledError | None = None
):
    cancellation = pending_cancellation
    while True:
        try:
            result = await asyncio.shield(owner)
        except asyncio.CancelledError as error:
            if owner.done() and owner.cancelled():
                if cancellation is not None:
                    cancellation.add_note("owned cleanup task was itself cancelled")
                    raise cancellation
                raise
            if cancellation is None:
                cancellation = error
            continue
        except BaseException:
            if cancellation is not None:
                cancellation.add_note("owned cleanup also failed")
                raise cancellation
            raise
        if cancellation is not None:
            raise cancellation
        return result


async def _run_owned_process(
    command: list[str], *, timeout_seconds: float
) -> OwnedProcessResult:
    process = await asyncio.create_subprocess_exec(
        *command,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env={"PATH": os.environ.get("PATH", "/usr/bin:/bin")},
    )
    try:
        stdout, stderr = await asyncio.wait_for(
            process.communicate(), timeout=timeout_seconds
        )
    except BaseException as error:
        owner = asyncio.create_task(_terminate_owned_process(process))
        cancellation = error if isinstance(error, asyncio.CancelledError) else None
        await _await_owned_task(owner, cancellation)
        raise
    assert process.returncode is not None
    return OwnedProcessResult(
        process.returncode,
        stdout.decode("utf-8", errors="replace"),
        stderr.decode("utf-8", errors="replace"),
    )


async def _collect_buffered_output_cancellation_safe(
    process: asyncio.subprocess.Process,
    *,
    timeout_sec: int | None,
    stdin_data: bytes | None = None,
) -> ExecResult:
    """Harbor-compatible collector that owns and reaps on every exit path."""
    try:
        if timeout_sec:
            stdout, stderr = await asyncio.wait_for(
                process.communicate(input=stdin_data), timeout=timeout_sec
            )
        else:
            stdout, stderr = await process.communicate(input=stdin_data)
    except BaseException as error:
        owner = asyncio.create_task(_terminate_owned_process(process))
        cancellation = error if isinstance(error, asyncio.CancelledError) else None
        try:
            await _await_owned_task(owner, cancellation)
        except asyncio.CancelledError:
            raise
        except BaseException as cleanup_error:
            raise ReadinessStageError(
                "compose collector", "reap_failed", "process_cleanup"
            ) from cleanup_error
        if isinstance(error, asyncio.TimeoutError):
            raise ReadinessStageError(
                "compose collector", "timeout", "deadline_exceeded"
            ) from error
        raise
    return ExecResult(
        stdout=stdout.decode(errors="replace") if stdout else None,
        stderr=stderr.decode(errors="replace") if stderr else None,
        return_code=process.returncode or 0,
    )


def _install_cancellation_safe_compose_collector(environment: object) -> None:
    """Bind the strict collector to one readiness environment instance only."""
    if not callable(getattr(environment, "_collect_buffered_output", None)):
        raise ReadinessStageError(
            "compose collector", "install_failed", "unsupported_environment"
        )
    environment._collect_buffered_output = _collect_buffered_output_cancellation_safe


def _validated_timeout_seconds(value: object, *, field: str) -> float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
    ):
        raise ReadinessError(f"{field} must be a finite positive number")
    return float(value)


def _healthcheck_timeout_bound(healthcheck: object | None) -> float:
    if healthcheck is None:
        return 0.0

    def duration(field: str, *, positive: bool = False) -> float:
        value = getattr(healthcheck, field, None)
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value < 0
            or (positive and value <= 0)
        ):
            raise ReadinessError(f"task healthcheck {field} is invalid")
        return float(value)

    timeout = duration("timeout_sec", positive=True)
    interval = duration("interval_sec")
    start_period = duration("start_period_sec")
    start_interval = duration("start_interval_sec")
    retries = getattr(healthcheck, "retries", None)
    if isinstance(retries, bool) or not isinstance(retries, int) or retries <= 0:
        raise ReadinessError("task healthcheck retries is invalid")
    # Harbor can begin one final start-period attempt immediately before the
    # boundary, then performs at most `retries` counted attempts afterward.
    bound = (
        start_period
        + timeout
        + start_interval
        + retries * timeout
        + (retries - 1) * interval
    )
    if not math.isfinite(bound):
        raise ReadinessError("task healthcheck timeout bound is invalid")
    return bound


def _task_tail_timeout_bound(
    *,
    build_timeout_seconds: float,
    dependency_setup_timeout_seconds: float,
    healthcheck_timeout_seconds: float,
) -> float:
    bound = (
        build_timeout_seconds
        + healthcheck_timeout_seconds
        + NETWORK_TRANSITIONS_PER_PROBE * NETWORK_TRANSITION_TIMEOUT_SECONDS
        + dependency_setup_timeout_seconds
        + CLEANUP_GRACE_SECONDS
        + TAIL_PROCESS_TERMINATION_SECONDS
    )
    if not math.isfinite(bound):
        raise ReadinessError("verifier readiness tail timeout bound is invalid")
    return bound


def _remaining_deadline_seconds(deadline: float, *, stage: str) -> float:
    remaining = deadline - asyncio.get_running_loop().time()
    if remaining <= 0:
        raise ReadinessError(f"verifier readiness {stage} deadline exceeded")
    return remaining


async def _await_tail_stage(
    operation,
    *,
    stage: str,
    timeout_seconds: float,
    tail_deadline: float,
):
    timeout = min(
        timeout_seconds,
        _remaining_deadline_seconds(tail_deadline, stage=stage),
    )
    try:
        return await asyncio.wait_for(operation(), timeout=timeout)
    except asyncio.TimeoutError as error:
        raise ReadinessStageError(stage, "timeout", "deadline_exceeded") from error
    except asyncio.CancelledError:
        raise
    except ReadinessStageError:
        raise
    except BaseException as error:
        raise ReadinessStageError(
            stage,
            "exception",
            _static_exception_category(error),
            subcategory=(
                error.subcategory
                if isinstance(error, (ReadinessContractError, DependencyProbeError))
                else None
            ),
        ) from error


def _static_exception_category(error: BaseException) -> str:
    if isinstance(error, ImageMaterializationError):
        return "image_materialization"
    if isinstance(error, ReadinessStageError):
        return error.category
    if isinstance(error, ReadinessError):
        return "readiness_contract"
    if isinstance(error, asyncio.TimeoutError):
        return "deadline_exceeded"
    if isinstance(error, OSError):
        return "operating_system"
    if isinstance(error, (ValueError, TypeError, json.JSONDecodeError)):
        return "invalid_input"
    if isinstance(error, RuntimeError):
        return "runtime"
    return "internal"


def _docker_error_category(value: str) -> str:
    """Classify raw Docker output without returning any attacker-controlled text."""
    normalized = value.casefold()
    for category, markers in (
        ("authorization_failed", ("unauthorized", "authentication required")),
        ("access_denied", ("requested access is denied", "permission denied")),
        ("not_found", ("manifest unknown", "no such image", "not found")),
        ("rate_limited", ("too many requests", "rate limit")),
        (
            "daemon_unavailable",
            ("cannot connect to the docker daemon", "is the docker daemon running"),
        ),
        (
            "network_timeout",
            ("tls handshake timeout", "i/o timeout", "context deadline exceeded"),
        ),
    ):
        if any(marker in normalized for marker in markers):
            return category
    return "unspecified"


def _process_failure_detail(result: OwnedProcessResult) -> str:
    category = _docker_error_category(f"{result.stderr}\n{result.stdout}")
    return (
        f"docker exited with status {result.returncode}; "
        f"category={category}"
    )


async def _run_image_command(
    command: list[str], *, stage: str, timeout_seconds: float
) -> OwnedProcessResult:
    try:
        return await _run_owned_process(command, timeout_seconds=timeout_seconds)
    except asyncio.TimeoutError as error:
        raise ImageMaterializationError(
            stage,
            "timeout",
            f"docker command exceeded {timeout_seconds:g} seconds",
        ) from error
    except OSError as error:
        error_number = error.errno if isinstance(error.errno, int) else "unknown"
        raise ImageMaterializationError(
            stage,
            "spawn",
            f"docker command could not be started; errno={error_number}",
        ) from error


def _parse_docker_info_string(result: OwnedProcessResult, *, field: str) -> str:
    if result.returncode != 0:
        raise ImageMaterializationError(
            "registry_transport", "nonzero_exit", _process_failure_detail(result)
        )
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ImageMaterializationError(
            "registry_transport", "invalid_output", f"docker info {field} is invalid"
        ) from error
    if not isinstance(value, str):
        raise ImageMaterializationError(
            "registry_transport", "invalid_output", f"docker info {field} is not a string"
        )
    return value


def _parse_primary_registry_mirror(result: OwnedProcessResult) -> str | None:
    if result.returncode != 0:
        raise ImageMaterializationError(
            "registry_transport", "nonzero_exit", _process_failure_detail(result)
        )
    try:
        mirrors = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ImageMaterializationError(
            "registry_transport", "invalid_output", "docker info registry mirrors are invalid"
        ) from error
    if not isinstance(mirrors, list) or not all(isinstance(item, str) for item in mirrors):
        raise ImageMaterializationError(
            "registry_transport", "invalid_output", "docker info registry mirrors are invalid"
        )
    if not mirrors:
        return None
    mirror = mirrors[0]
    parsed = urlsplit(mirror)
    if (
        parsed.scheme != "https"
        or not parsed.netloc
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise ImageMaterializationError(
            "registry_transport", "invalid_output", "primary registry mirror URL is unsafe"
        )
    return mirror.rstrip("/") + "/v2/"


def _probe_registry_endpoint(url: str, proxy: str) -> None:
    proxies = {"http": proxy, "https": proxy} if proxy else {}
    opener = urllib.request.build_opener(urllib.request.ProxyHandler(proxies))
    request = urllib.request.Request(url, method="GET")
    try:
        with opener.open(request, timeout=REGISTRY_TRANSPORT_TIMEOUT_SECONDS) as response:
            status = response.status
    except urllib.error.HTTPError as error:
        status = error.code
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        reason = getattr(error, "reason", error)
        category = "tls" if "CERTIFICATE_VERIFY_FAILED" in str(reason) else "network"
        raise ImageMaterializationError(
            "registry_transport", "unreachable", f"primary registry mirror {category} probe failed"
        ) from error
    if not 200 <= status < 500:
        raise ImageMaterializationError(
            "registry_transport", "unreachable", "primary registry mirror returned an unavailable status"
        )


async def _probe_primary_registry_transport() -> None:
    """Fail fast when dockerd's configured primary mirror cannot serve /v2/."""
    mirrors = await _run_owned_process(
        ["docker", "info", "--format", "{{json .RegistryConfig.Mirrors}}"],
        timeout_seconds=IMAGE_INSPECT_TIMEOUT_SECONDS,
    )
    endpoint = _parse_primary_registry_mirror(mirrors)
    if endpoint is None:
        return
    proxy_result = await _run_owned_process(
        ["docker", "info", "--format", "{{json .HTTPSProxy}}"],
        timeout_seconds=IMAGE_INSPECT_TIMEOUT_SECONDS,
    )
    proxy = _parse_docker_info_string(proxy_result, field="HTTPSProxy")
    await asyncio.to_thread(_probe_registry_endpoint, endpoint, proxy)


def _parse_image_inspection(
    image: OwnedProcessResult, *, stage: str
) -> tuple[str, list[str]]:
    if image.returncode != 0:
        raise ImageMaterializationError(
            stage, "nonzero_exit", _process_failure_detail(image)
        )
    try:
        payload = json.loads(image.stdout)
        if not isinstance(payload, list) or not payload:
            raise ValueError("top-level inspection result is not a non-empty list")
        image_value = payload[0]
        if not isinstance(image_value, dict):
            raise ValueError("inspection result is not an object")
    except (json.JSONDecodeError, ValueError) as error:
        raise ImageMaterializationError(
            stage,
            "invalid_output",
            f"docker inspect returned invalid JSON ({type(error).__name__})",
        ) from error
    image_id = image_value.get("Id")
    raw_repo_digests = image_value.get("RepoDigests")
    if not isinstance(raw_repo_digests, list):
        raise ImageMaterializationError(
            stage, "invalid_output", "RepoDigests is not a list"
        )
    if not all(
        isinstance(value, str)
        and re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", value) is not None
        for value in raw_repo_digests
    ):
        raise ImageMaterializationError(
            stage, "invalid_output", "RepoDigests contains a malformed value"
        )
    repo_digests = sorted(raw_repo_digests)
    if re.fullmatch(r"sha256:[0-9a-f]{64}", str(image_id)) is None:
        raise ImageMaterializationError(
            stage, "invalid_output", "verifier image is not content-addressed"
        )
    return str(image_id), repo_digests


async def _inspect_image(
    image_reference: str,
    *,
    pull_timeout_seconds: float,
    materialization_semaphore: asyncio.Semaphore | None = None,
) -> tuple[str, list[str], str]:
    """Resolve an authoritative image, never trusting a mutable local tag."""
    pull_timeout_seconds = _validated_timeout_seconds(
        pull_timeout_seconds, field="task environment build_timeout_sec"
    )
    semaphore = materialization_semaphore or asyncio.Semaphore(
        IMAGE_MATERIALIZATION_CONCURRENCY
    )
    digest_pinned = "@sha256:" in image_reference
    if digest_pinned:
        image = await _run_image_command(
            ["docker", "image", "inspect", image_reference],
            stage="cache_inspect",
            timeout_seconds=IMAGE_INSPECT_TIMEOUT_SECONDS,
        )
        if image.returncode == 0:
            image_id, repo_digests = _parse_image_inspection(
                image, stage="cache_inspect"
            )
            return image_id, repo_digests, "digest-pinned-cache"

    # Pulls share dockerd and can serialize internally. Do not let task-probe
    # concurrency multiply that contention. Holding the semaphore through the
    # post-pull inspect also makes the mutable-tag -> RepoDigest binding atomic
    # with respect to other readiness pulls in this process.
    async with semaphore:
        if digest_pinned:
            image = await _run_image_command(
                ["docker", "image", "inspect", image_reference],
                stage="cache_reinspect",
                timeout_seconds=IMAGE_INSPECT_TIMEOUT_SECONDS,
            )
            if image.returncode == 0:
                image_id, repo_digests = _parse_image_inspection(
                    image, stage="cache_reinspect"
                )
                return image_id, repo_digests, "digest-pinned-cache"
        pull = await _run_image_command(
            ["docker", "image", "pull", image_reference],
            stage="pull",
            timeout_seconds=pull_timeout_seconds,
        )
        if pull.returncode != 0:
            raise ImageMaterializationError(
                "pull", "nonzero_exit", _process_failure_detail(pull)
            )
        image = await _run_image_command(
            ["docker", "image", "inspect", image_reference],
            stage="post_inspect",
            timeout_seconds=IMAGE_INSPECT_TIMEOUT_SECONDS,
        )
        image_id, repo_digests = _parse_image_inspection(
            image, stage="post_inspect"
        )
        return image_id, repo_digests, "pulled"


def _write_cleanup_record(environment: object, state_dir: Path) -> None:
    project = _sanitize_docker_compose_project_name(environment.session_id)
    source_compose_files = [
        path.resolve().absolute() for path in environment._docker_compose_paths
    ]
    if not source_compose_files:
        raise ReadinessError(
            "Harbor environment has no exact Docker Compose definition"
        )
    try:
        compose_env = project_recovery_compose_env(
            environment._compose_env_vars(include_os_env=False)
        )
    except ValueError as error:
        raise ReadinessError(str(error)) from error
    directory = state_dir / "docker-projects"
    if directory.is_symlink() or not directory.is_dir():
        raise ReadinessError("lifecycle Docker project ledger is unavailable")
    compose_directory = state_dir / "docker-compose-records" / project
    compose_directory.mkdir(parents=True, mode=0o700)
    compose_files: list[str] = []
    for index, source in enumerate(source_compose_files):
        if source.is_symlink() or not source.is_file():
            raise ReadinessError(f"Harbor Compose definition is unavailable: {source}")
        target = compose_directory / f"{index:02d}.yaml"
        descriptor = os.open(
            target,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o400,
        )
        with (
            source.open("rb") as input_stream,
            os.fdopen(descriptor, "wb") as output_stream,
        ):
            while block := input_stream.read(1024 * 1024):
                output_stream.write(block)
            output_stream.flush()
            os.fsync(output_stream.fileno())
        compose_files.append(str(target))
    record = directory / f"{project}.json"
    payload = {
        "schema": "astra.harness.docker_project.v1",
        "project": project,
        "project_directory": str(environment.environment_dir.resolve().absolute()),
        "compose_files": compose_files,
        "compose_env": compose_env,
    }
    descriptor = os.open(
        record,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o400,
    )
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(payload, stream, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def _install_compose_registration(environment: object, state_dir: Path) -> None:
    original = environment._run_docker_compose_command
    registered = False

    async def managed(command: list[str], *args: object, **kwargs: object):
        nonlocal registered
        if not registered:
            _write_cleanup_record(environment, state_dir)
            registered = True
        return await original(command, *args, **kwargs)

    environment._run_docker_compose_command = managed


async def _assert_project_quiescent(environment: object) -> None:
    project = _sanitize_docker_compose_project_name(environment.session_id)
    errors: list[Exception] = []
    for kind, command in (
        ("containers", ["ps", "--all"]),
        ("networks", ["network", "ls"]),
        ("volumes", ["volume", "ls"]),
    ):
        try:
            result = await _run_owned_process(
                [
                    "docker",
                    *command,
                    "--quiet",
                    "--filter",
                    f"label=com.docker.compose.project={project}",
                ],
                timeout_seconds=15,
            )
        except (OSError, asyncio.TimeoutError) as error:
            errors.append(
                ReadinessStageError(
                    f"cleanup quiescence {kind}",
                    "exception",
                    _static_exception_category(error),
                )
            )
            continue
        if result.returncode != 0:
            errors.append(
                ReadinessError(f"cannot prove verifier readiness {kind} cleanup")
            )
        elif result.stdout.strip():
            errors.append(
                ReadinessError(f"verifier readiness {kind} remain after cleanup")
            )
    if errors:
        raise errors[0]


async def _strict_delete_environment(
    environment: object,
    *,
    cleanup_grace_seconds: float = CLEANUP_GRACE_SECONDS,
    tail_deadline: float | None = None,
) -> None:
    """Attempt every cleanup layer under one shared whole-cleanup deadline."""
    cleanup_grace_seconds = _validated_timeout_seconds(
        cleanup_grace_seconds, field="verifier readiness cleanup timeout"
    )
    loop = asyncio.get_running_loop()
    cleanup_deadline = loop.time() + cleanup_grace_seconds
    if tail_deadline is not None:
        cleanup_deadline = min(cleanup_deadline, tail_deadline)
    errors: list[tuple[str, BaseException]] = []

    async def attempt(operation, *, stage: str, stages_remaining: int) -> None:
        try:
            remaining = _remaining_deadline_seconds(
                cleanup_deadline, stage="cleanup"
            )
            # Reserve an equal share for every not-yet-attempted asynchronous
            # layer. Unused time flows forward to later cleanup stages.
            await asyncio.wait_for(
                operation(), timeout=remaining / stages_remaining
            )
        except (Exception, asyncio.CancelledError) as error:
            errors.append((stage, error))

    await attempt(
        environment.prepare_logs_for_host,
        stage="prepare_logs",
        stages_remaining=3,
    )
    await attempt(
        lambda: environment._run_docker_compose_command(
            ["down", "--rmi", "local", "--volumes", "--remove-orphans"]
        ),
        stage="compose_down",
        stages_remaining=2,
    )
    for stage, cleanup in (
        ("cleanup_mounts", environment._cleanup_mounts_compose_file),
        ("cleanup_resources", environment._cleanup_resources_compose_file),
        ("cleanup_env", environment._cleanup_env_compose_file),
        (
            "cleanup_egress",
            environment._cleanup_egress_control_services_compose_file,
        ),
    ):
        try:
            cleanup()
        except Exception as error:
            errors.append((stage, error))
    if loop.time() >= cleanup_deadline:
        errors.append(
            (
                "whole_cleanup",
                ReadinessStageError(
                    "cleanup", "timeout", "deadline_exceeded"
                ),
            )
        )
    await attempt(
        lambda: _assert_project_quiescent(environment),
        stage="quiescence",
        stages_remaining=1,
    )
    if errors:
        stage, first = errors[0]
        if isinstance(first, ReadinessStageError):
            raise first
        raise ReadinessStageError(
            f"cleanup {stage}",
            "exception",
            _static_exception_category(first),
        ) from first


async def _await_cleanup_owner(
    environment: object, *, tail_deadline: float | None = None
) -> None:
    owner = asyncio.create_task(
        _strict_delete_environment(environment, tail_deadline=tail_deadline)
    )
    await _await_owned_task(owner)


@contextlib.asynccontextmanager
async def _verifier_phase(
    environment: object,
    baseline: object,
    phase: object,
    *,
    tail_deadline: float | None = None,
):
    if phase == baseline:
        yield
        return
    deadline = (
        tail_deadline
        if tail_deadline is not None
        else asyncio.get_running_loop().time()
        + 2 * NETWORK_TRANSITION_TIMEOUT_SECONDS
    )
    await _await_tail_stage(
        lambda: environment.set_network_policy(phase),
        stage="network transition to verifier phase",
        timeout_seconds=NETWORK_TRANSITION_TIMEOUT_SECONDS,
        tail_deadline=deadline,
    )
    try:
        yield
    finally:
        owner = asyncio.create_task(
            _await_tail_stage(
                lambda: environment.set_network_policy(baseline),
                stage="network transition to baseline",
                timeout_seconds=NETWORK_TRANSITION_TIMEOUT_SECONDS,
                tail_deadline=deadline,
            )
        )
        await _await_owned_task(owner)


async def _probe_verifier_container(
    environment: object,
    projection: dict[str, str],
    environment_paths: EnvironmentPaths,
    tests_source_dir: Path,
    test_path: Path,
    verifier_owns_tests: bool,
    dependency_setup_timeout_seconds: float,
) -> dict[str, object]:
    """Execute the statically proven dependency prefix, never the scorer."""
    invocations: list[dict[str, object]] = []
    invocation_subcategories = {
        "readability_probe": "dependency_readability",
        "dependency_setup": "dependency_batch_exec",
        "fixture_workdir_probe": "dependency_fixture_workdir",
        "fixture_destination_probe": "dependency_fixture_destination",
        "fixture_stat": "dependency_fixture_stat",
        "fixture_digest": "dependency_fixture_digest",
        "source_resolve": "dependency_source_resolve",
        "source_stat_before": "dependency_source_stat",
        "source_stat_after": "dependency_source_stat",
        "source_digest_before": "dependency_source_digest",
        "source_digest_after": "dependency_source_digest",
    }

    async def invoke(kind: str, command: str):
        with _dependency_probe_boundary(invocation_subcategories[kind]):
            result = await environment.exec(
                command=command,
                user=environment.default_user,
                env=projection,
            )
        invocations.append(
            {
                "sequence": len(invocations),
                "kind": kind,
                "command_sha256": hashlib.sha256(command.encode()).hexdigest(),
                "exit_code": result.return_code,
            }
        )
        return result

    if verifier_owns_tests:
        relative = test_path.name
    else:
        with _dependency_probe_boundary("dependency_readability"):
            await environment.upload_dir(
                source_dir=tests_source_dir,
                target_dir=str(environment_paths.tests_dir),
            )
        with _dependency_probe_boundary("dependency_readability"):
            relative = test_path.relative_to(tests_source_dir).as_posix()
    remote_test_path = environment_paths.tests_dir / relative
    result = await invoke("readability_probe", f"test -r {remote_test_path}")
    if result.return_code != 0:
        raise DependencyProbeError("dependency_readability")

    with _dependency_probe_boundary("dependency_readability"):
        script = test_path.read_text(encoding="utf-8", errors="replace")
    setup_plan = build_dependency_setup_plan(
        script, test_path, tests_source_dir=tests_source_dir
    )
    source_indexes = [
        index
        for index, step in enumerate(setup_plan.steps)
        if step.kind == "environment_source"
    ]
    # Each source is inspected outside the setup shell, so stateful shell
    # changes made before that boundary would be lost. Stateful steps after
    # the final source stay in the final setup batch and are safe to execute.
    if source_indexes and any(
        step.kind == "environment"
        for step in setup_plan.steps[: source_indexes[-1]]
    ):
        raise DependencyProbeError("dependency_source_policy")
    batches: list[dict[str, object]] = []
    sources: list[dict[str, object]] = []
    fixtures: list[dict[str, object]] = []
    environment_delta = EnvironmentDelta()
    venv_digests: dict[int, str] = {}
    batch_start = 0

    fixture_workdir: str | None = None

    async def execute_fixture(fixture: FixtureStage) -> None:
        nonlocal fixture_workdir
        if fixture_workdir is None:
            workdir_result = await invoke("fixture_workdir_probe", "pwd -P")
            with _dependency_probe_boundary("dependency_fixture_workdir"):
                fixture_workdir = _parse_fixture_workdir(
                    workdir_result, environment_paths
                )
        workdir = fixture_workdir
        local_source = tests_source_dir.joinpath(
            *PurePosixPath(fixture.source_relative).parts
        )
        with _dependency_probe_boundary("dependency_fixture_source"):
            if (
                _authoritative_fixture_sha256(local_source, tests_source_dir)
                != fixture.source_sha256
            ):
                raise ReadinessError("fixture source identity changed")
        source_path = environment_paths.tests_dir / fixture.source_relative
        destination = PurePosixPath(workdir) / fixture.basename
        destination_probe = (
            f"test ! -e {shlex.quote(str(destination))} && "
            f"test ! -L {shlex.quote(str(destination))}"
        )
        absent = await invoke("fixture_destination_probe", destination_probe)
        if absent.return_code != 0:
            raise DependencyProbeError("dependency_fixture_destination")
        with _dependency_probe_boundary("dependency_fixture_upload"):
            await environment.upload_file(local_source, str(destination))
        stat_command = (
            f"test -f {shlex.quote(str(destination))} && "
            f"test ! -L {shlex.quote(str(destination))} && "
            f"{_source_stat_command(str(destination))}"
        )
        with _dependency_probe_boundary("dependency_fixture_stat"):
            identity = _parse_regular_file_identity(
                await invoke("fixture_stat", stat_command)
            )
        digest_command = f"sha256sum -- {shlex.quote(str(destination))}"
        with _dependency_probe_boundary("dependency_fixture_digest"):
            destination_digest = _parse_source_content_digest(
                await invoke("fixture_digest", digest_command)
            )
        with _dependency_probe_boundary("dependency_fixture_content"):
            if (
                destination_digest != fixture.source_sha256
                or identity[2] != local_source.stat().st_size
            ):
                raise ReadinessError("fixture destination identity changed")
        fixtures.append(
            {
                "sequence": fixture.sequence,
                "step_index": fixture.step_index,
                "cwd_sha256": hashlib.sha256(workdir.encode()).hexdigest(),
                "source_sha256": hashlib.sha256(str(source_path).encode()).hexdigest(),
                "destination_sha256": hashlib.sha256(
                    str(destination).encode()
                ).hexdigest(),
                "content_sha256": destination_digest,
                "content_bytes": identity[2],
                "destination_probe_command_sha256": hashlib.sha256(
                    destination_probe.encode()
                ).hexdigest(),
                "stat_command_sha256": hashlib.sha256(
                    stat_command.encode()
                ).hexdigest(),
                "digest_command_sha256": hashlib.sha256(
                    digest_command.encode()
                ).hexdigest(),
            }
        )

    async def execute_batch(start: int, end: int) -> None:
        with _dependency_probe_boundary("dependency_batch_exec"):
            command = _render_dependency_setup_batch(
                setup_plan, start, end, environment_delta
            )
        setup_result = await invoke("dependency_setup", command)
        if setup_result.return_code != 0:
            raise DependencyProbeError("dependency_batch_exec")
        with _dependency_probe_boundary("dependency_batch_receipt"):
            completed_steps = _completed_setup_steps(setup_result.stdout or "")
            if completed_steps != list(range(start, end)):
                raise ReadinessError(
                    "verifier dependency setup step receipts are incomplete"
                )
            exit_codes = _completed_setup_exit_codes(setup_result.stdout or "")
            if sorted(exit_codes) != list(range(start, end)):
                raise ReadinessError(
                    "verifier dependency setup exit receipts are incomplete"
                )
            output = setup_result.stdout or ""
            for index in range(start, end):
                step = setup_plan.steps[index]
                if step.kind != "venv_create":
                    continue
                marker = f"{VENV_DIGEST_PREFIX}{index}"
                marker_position = output.find(marker)
                if marker_position < 0:
                    raise ReadinessError(
                        "verifier virtualenv activation identity is unavailable"
                    )
                digest_lines = output[marker_position:].splitlines()[1:]
                digest = next(
                    (
                        line.split(maxsplit=1)[0]
                        for line in digest_lines
                        if re.fullmatch(r"[0-9a-f]{64}\s+.+", line)
                    ),
                    None,
                )
                if digest is None:
                    raise ReadinessError(
                        "verifier virtualenv activation identity is unavailable"
                    )
                venv_digests[index] = digest
        batches.append(
            {
                "start": start,
                "end": end,
                "command_sha256": hashlib.sha256(command.encode()).hexdigest(),
                "step_exit_codes": [exit_codes[index] for index in range(start, end)],
            }
        )

    fixtures_by_step = {fixture.step_index: fixture for fixture in setup_plan.fixtures}
    for index, step in enumerate(setup_plan.steps):
        if step.kind not in {"environment_source", "fixture_stage"}:
            continue
        if batch_start < index:
            await execute_batch(batch_start, index)
        if step.kind == "fixture_stage":
            fixture = fixtures_by_step.get(index)
            if fixture is None:
                raise DependencyProbeError("dependency_fixture_source")
            await execute_fixture(fixture)
            batch_start = index + 1
            continue
        with _dependency_probe_boundary("dependency_source_resolve"):
            expression = _source_path_expression(step.command)
            resolve_command = _source_resolve_command(expression)
        resolved = await invoke("source_resolve", resolve_command)
        with _dependency_probe_boundary("dependency_source_resolve"):
            canonical_lines = (resolved.stdout or "").splitlines()
            if resolved.return_code != 0 or len(canonical_lines) != 1:
                raise ReadinessError("environment source path cannot be resolved")
            canonical_path = canonical_lines[0]
            canonical = PurePosixPath(canonical_path)
            if not canonical.is_absolute() or ".." in canonical.parts:
                raise ReadinessError("environment source canonical path is invalid")
        stat_command = _source_stat_command(canonical_path)
        stat_before = await invoke("source_stat_before", stat_command)
        with _dependency_probe_boundary("dependency_source_stat"):
            identity_before = _parse_source_file_identity(stat_before)
        digest_command = f"sha256sum -- {shlex.quote(canonical_path)}"
        with _dependency_probe_boundary("dependency_source_digest"):
            digest_before = _parse_source_content_digest(
                await invoke("source_digest_before", digest_command)
            )
        with (
            _dependency_probe_boundary("dependency_source_policy"),
            tempfile.TemporaryDirectory(
                prefix="astra-verifier-source-"
            ) as directory,
        ):
            local_source = Path(directory) / "source"
            with _dependency_probe_boundary("dependency_source_policy"):
                await environment.download_file(canonical_path, local_source)
            stat_after = await invoke("source_stat_after", stat_command)
            with _dependency_probe_boundary("dependency_source_stat"):
                identity_after = _parse_source_file_identity(stat_after)
                if identity_after != identity_before:
                    raise ReadinessError(
                        "environment source identity changed while reading"
                    )
            with _dependency_probe_boundary("dependency_source_digest"):
                digest_after = _parse_source_content_digest(
                    await invoke("source_digest_after", digest_command)
                )
            with _dependency_probe_boundary("dependency_source_policy"):
                content = local_source.read_bytes()
        with _dependency_probe_boundary("dependency_source_policy"):
            if len(content) != identity_before[2]:
                raise ReadinessError("environment source changed while being read")
        content_sha256 = hashlib.sha256(content).hexdigest()
        with _dependency_probe_boundary("dependency_source_digest"):
            if digest_before != digest_after or digest_after != content_sha256:
                raise ReadinessError(
                    "environment source content changed while reading"
                )
        with _dependency_probe_boundary("dependency_source_policy"):
            creator_target: str | None = None
            creator_digest: str | None = None
            for creator_index in range(index - 1, -1, -1):
                creator = setup_plan.steps[creator_index]
                if creator.kind != "venv_create":
                    continue
                candidate_target = _parse_uv_venv_target(creator.command)
                if not _venv_source_matches_creator(
                    expression, canonical_path, candidate_target
                ):
                    continue
                if any(
                    setup_plan.steps[intermediate].kind
                    not in {"environment_guard"}
                    for intermediate in range(creator_index + 1, index)
                ):
                    raise ReadinessError(
                        "environment source is not creator-bound"
                    )
                creator_target = candidate_target
                creator_digest = venv_digests.get(creator_index)
                break
            if creator_target is not None:
                if creator_digest is None or creator_digest != content_sha256:
                    raise ReadinessError(
                        "virtualenv activation identity changed while reading"
                    )
            source_delta = _parse_environment_source(
                content,
                expression,
                canonical_path,
                creator_target=creator_target,
                creator_digest=creator_digest,
            )
        with _dependency_probe_boundary("dependency_source_policy"):
            environment_delta = environment_delta.merged(source_delta)
        sources.append(
            {
                "step_index": index,
                "canonical_path": canonical_path,
                "device": identity_before[0],
                "inode": identity_before[1],
                "content_sha256": content_sha256,
                "content_bytes": len(content),
                "environment_delta_sha256": source_delta.receipt_sha256(),
                "resolve_command_sha256": hashlib.sha256(
                    resolve_command.encode()
                ).hexdigest(),
                "stat_command_sha256": hashlib.sha256(
                    stat_command.encode()
                ).hexdigest(),
                "digest_command_sha256": hashlib.sha256(
                    digest_command.encode()
                ).hexdigest(),
            }
        )
        batch_start = index + 1
    if batch_start < len(setup_plan.steps):
        await execute_batch(batch_start, len(setup_plan.steps))
    receipt_plan = setup_plan.receipt_plan()
    scoring_invoked = any(
        invocation["kind"] == "scoring"
        or invocation["command_sha256"] == setup_plan.scoring_command_sha256
        for invocation in invocations
    )
    return {
        "mode": setup_plan.mode,
        "plan": receipt_plan,
        "plan_sha256": canonical_json_sha256(receipt_plan),
        "budget_seconds": dependency_setup_timeout_seconds,
        "invocations": invocations,
        "batches": batches,
        "batches_sha256": canonical_json_sha256(batches),
        "sources": sources,
        "sources_sha256": canonical_json_sha256(sources),
        "fixtures": fixtures,
        "fixtures_sha256": canonical_json_sha256(fixtures),
        "executions": [
            {
                "index": index,
                "kind": step["kind"],
                "command_sha256": step["command_sha256"],
                "exit_code": 0,
            }
            for index, step in enumerate(receipt_plan["steps"])
        ],
        "scoring_invoked": scoring_invoked,
    }


def _parse_fixture_workdir(
    result: object, environment_paths: EnvironmentPaths
) -> str:
    lines = (result.stdout or "").splitlines()
    if result.return_code != 0 or len(lines) != 1:
        raise ReadinessError("fixture workdir is unavailable")
    raw = lines[0]
    path = PurePosixPath(raw)
    forbidden = (
        environment_paths.tests_dir,
        environment_paths.logs_dir,
        environment_paths.solution_dir,
    )
    if (
        not raw
        or any(ord(character) < 0x20 or ord(character) == 0x7F for character in raw)
        or not path.is_absolute()
        or path == PurePosixPath("/")
        or path.as_posix() != raw
        or any(path == root or path.is_relative_to(root) for root in forbidden)
    ):
        raise ReadinessError("fixture workdir is unavailable")
    return raw


STATIC_BINDING_RESERVED_NAMES = {
    "BASH_ENV",
    "CDPATH",
    "ENV",
    "HOME",
    "IFS",
    "PATH",
    "PWD",
    "SHELL",
}


def _parse_static_binding_declaration(unit: str) -> StaticBinding | None:
    if re.match(r"^export\s+", unit):
        # Exported assignments are not planner bindings.  Route them through
        # the existing closed SAFE_LITERAL_ASSIGNMENTS command classifier.
        return None
    assignment = re.fullmatch(
        r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)=(?P<raw>.*)", unit
    )
    if assignment is None:
        if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", unit):
            raise ReadinessContractError("plan_static_binding_disallowed")
        return None
    tokens = _shell_tokens(unit)
    # An assignment prefix belongs to the existing command classifier.  This
    # declaration grammar owns only a complete, standalone shell unit.
    if len(tokens) != 1:
        return None
    name = assignment.group("name")
    raw = assignment.group("raw")
    if (
        name in STATIC_BINDING_RESERVED_NAMES
        or name.startswith(("LD_", "DYLD_"))
        or _credential_shaped_name(name)
        or not raw
        or len(raw.encode("utf-8")) > MAX_STATIC_BINDING_VALUE_BYTES
        or any(character in raw for character in ("$", "`", "\\", "\0"))
        or any(ord(character) < 0x20 or ord(character) == 0x7F for character in raw)
    ):
        raise ReadinessContractError("plan_static_binding_disallowed")
    try:
        value = _parse_static_source_value(raw)
    except ReadinessError as error:
        raise ReadinessContractError("plan_static_binding_disallowed") from error
    if re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ReadinessContractError("plan_static_binding_disallowed")
    return StaticBinding(
        name=name,
        value=value,
        assignment_sha256=hashlib.sha256(unit.encode()).hexdigest(),
    )


def _binding_expansion_bodies(command: str) -> list[str]:
    return re.findall(r"\$\{([^}]*)\}", command)


def _resolve_static_scoring_bindings(
    command: str, bindings: dict[str, StaticBinding]
) -> tuple[str, set[str], str]:
    if any(token in command for token in ("$(", "`", "\\")):
        raise ReadinessContractError("plan_static_binding_disallowed")
    tokens = _shell_tokens(command)
    if not tokens:
        raise ReadinessContractError("plan_static_binding_disallowed")
    if tokens[0] == "uvx":
        scorer_index = _uv_scorer_index(tokens, 1)
    elif len(tokens) >= 2 and tokens[:2] == ["uv", "run"]:
        scorer_index = _uv_scorer_index(tokens, 2)
    else:
        raise ReadinessContractError("plan_static_binding_disallowed")

    used: set[str] = set()
    resolved_tokens = list(tokens)
    for index, token in enumerate(tokens):
        bodies = re.findall(r"\$\{([^}]*)\}", token)
        plain_names = re.findall(r"\$([A-Za-z_][A-Za-z0-9_]*)", token)
        if plain_names:
            raise ReadinessContractError("plan_static_binding_disallowed")
        without_exact = re.sub(r"\$\{[A-Za-z_][A-Za-z0-9_]*\}", "", token)
        if "$" in without_exact:
            raise ReadinessContractError("plan_static_binding_disallowed")
        if not bodies:
            continue
        if index >= scorer_index or index == 0:
            raise ReadinessContractError("plan_static_binding_disallowed")
        if index == 0 or tokens[index - 1] not in {"-w", "--with"}:
            raise ReadinessContractError("plan_static_binding_disallowed")
        if len(bodies) != 1 or re.fullmatch(
            r"git\+https://[^\s]+@\$\{[A-Za-z_][A-Za-z0-9_]*\}", token
        ) is None:
            raise ReadinessContractError("plan_static_binding_disallowed")
        resolved = token
        for body in bodies:
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", body) is None:
                raise ReadinessContractError("plan_static_binding_disallowed")
            binding = bindings.get(body)
            if binding is None:
                raise ReadinessContractError("plan_static_binding_disallowed")
            resolved = resolved.replace(f"${{{body}}}", binding.value)
            used.add(body)
        parsed = urlsplit(resolved.removeprefix("git+"))
        if (
            parsed.scheme != "https"
            or not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
            or re.search(r"@[0-9a-f]{40}$", parsed.path) is None
        ):
            raise ReadinessContractError("plan_static_binding_disallowed")
        resolved_tokens[index] = resolved
    if not used:
        raise ReadinessContractError("plan_static_binding_disallowed")
    return (
        shlex.join(resolved_tokens),
        used,
        canonical_json_sha256(resolved_tokens),
    )


def _authoritative_fixture_sha256(candidate: Path, tests_dir: Path) -> str:
    try:
        root_status = tests_dir.lstat()
        if not stat.S_ISDIR(root_status.st_mode) or stat.S_ISLNK(root_status.st_mode):
            raise ReadinessContractError(
                "plan_unclassified_pre_scoring_command"
            )
        relative = candidate.relative_to(tests_dir)
        current = tests_dir
        for index, part in enumerate(relative.parts):
            current = current / part
            status = current.lstat()
            if stat.S_ISLNK(status.st_mode) or (
                index < len(relative.parts) - 1 and not stat.S_ISDIR(status.st_mode)
            ):
                raise ReadinessContractError(
                    "plan_unclassified_pre_scoring_command"
                )
        before = candidate.lstat()
        if not stat.S_ISREG(before.st_mode):
            raise ReadinessContractError(
                "plan_unclassified_pre_scoring_command"
            )
        descriptor = os.open(
            candidate,
            os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
        )
        try:
            opened = os.fstat(descriptor)
            if (opened.st_dev, opened.st_ino, opened.st_size) != (
                before.st_dev,
                before.st_ino,
                before.st_size,
            ) or not stat.S_ISREG(opened.st_mode):
                raise ReadinessContractError(
                    "plan_unclassified_pre_scoring_command"
                )
            digest = hashlib.sha256()
            while chunk := os.read(descriptor, 1024 * 1024):
                digest.update(chunk)
            after = candidate.lstat()
            if (after.st_dev, after.st_ino, after.st_size) != (
                before.st_dev,
                before.st_ino,
                before.st_size,
            ):
                raise ReadinessContractError(
                    "plan_unclassified_pre_scoring_command"
                )
            return digest.hexdigest()
        finally:
            os.close(descriptor)
    except OSError as error:
        raise ReadinessContractError(
            "plan_unclassified_pre_scoring_command"
        ) from error


def _classify_fixture_stage(
    command: str, tests_dir: Path, sequence: int, step_index: int
) -> FixtureStage | None:
    tokens = _shell_tokens(command)
    if not tokens or tokens[0] != "cp":
        return None
    if (
        len(tokens) != 3
        or any(token in command for token in ("$", "`", "\\"))
        or _has_unquoted_setup_metacharacter(command)
        or any(token.startswith("-") for token in tokens[1:])
        or tokens[2] != "."
    ):
        raise ReadinessContractError("plan_unclassified_pre_scoring_command")
    source = PurePosixPath(tokens[1])
    tests_root = PurePosixPath("/tests")
    try:
        relative = source.relative_to(tests_root)
    except ValueError as error:
        raise ReadinessContractError(
            "plan_unclassified_pre_scoring_command"
        ) from error
    if (
        not relative.parts
        or ".." in relative.parts
        or source.name in {"", ".", ".."}
        or source.as_posix().endswith("/")
    ):
        raise ReadinessContractError("plan_unclassified_pre_scoring_command")
    candidate = tests_dir.joinpath(*relative.parts)
    return FixtureStage(
        sequence=sequence,
        step_index=step_index,
        source_relative=relative.as_posix(),
        basename=source.name,
        source_sha256=_authoritative_fixture_sha256(candidate, tests_dir),
    )


def build_dependency_setup_plan(
    verifier_script: str,
    test_path: Path,
    tests_source_dir: Path | None = None,
) -> DependencySetupPlan:
    """Compile a no-score dependency plan from a conservative shell grammar.

    The compiler is intentionally fail-closed.  It understands dependency
    command *families*, not benchmark names, and refuses dynamic shell syntax
    that could hide either setup work or scoring side effects.
    """
    supported_shell = test_path.suffix in {".sh", ".bash"} or re.match(
        r"^#!\s*(?:/usr/bin/env\s+)?(?:/\S*/)?(?:sh|bash)(?:\s|$)",
        verifier_script,
    )
    if not supported_shell:
        raise ReadinessError("dependency setup requires a supported shell verifier")

    units = _logical_shell_units(verifier_script)
    steps: list[DependencySetupStep] = []
    fixtures: list[FixtureStage] = []
    runner_family: str | None = None
    scoring_command_sha256: str | None = None
    scoring_boundary = False
    bindings: dict[str, StaticBinding] = {}
    used_bindings: set[str] = set()
    authoritative_tests = tests_source_dir or test_path.parent
    for sequence, unit in enumerate(units):
        binding = _parse_static_binding_declaration(unit)
        fixture = _classify_fixture_stage(
            unit, authoritative_tests, sequence, len(steps)
        )
        if scoring_boundary:
            if binding is not None or fixture is not None:
                raise ReadinessContractError("plan_static_binding_disallowed")
            if _contains_dependency_intent(unit):
                raise ReadinessError(
                    "dependency setup appears after the scoring boundary"
                )
            continue

        if binding is not None:
            if binding.name in bindings:
                raise ReadinessContractError("plan_static_binding_disallowed")
            bindings[binding.name] = binding
            continue

        referenced_bodies = _binding_expansion_bodies(unit)
        references_static_binding = any(
            body != "HOME" for body in referenced_bodies
        ) or any(
            re.search(rf"\${re.escape(name)}\b", unit) is not None
            for name in bindings
        )
        resolved_unit = unit
        unit_used_bindings: set[str] = set()
        if references_static_binding:
            (
                resolved_unit,
                unit_used_bindings,
                _,
            ) = _resolve_static_scoring_bindings(unit, bindings)

        if unit.startswith("if "):
            if references_static_binding:
                raise ReadinessContractError("plan_static_binding_disallowed")
            if not _is_safe_environment_guard(unit):
                raise ReadinessError("cannot safely parse verifier control flow")
            steps.append(DependencySetupStep("environment_guard", unit))
            continue

        if fixture is not None:
            if references_static_binding:
                raise ReadinessContractError("plan_static_binding_disallowed")
            if fixture.basename in {item.basename for item in fixtures} or any(
                _shell_tokens(step.command)[:1] in (["cd"], ["pushd"], ["popd"])
                for step in steps
            ):
                raise ReadinessContractError(
                    "plan_unclassified_pre_scoring_command"
                )
            fixtures.append(fixture)
            steps.append(
                DependencySetupStep(
                    "fixture_stage",
                    canonical_json_sha256(fixture.receipt()),
                )
            )
            continue

        if not references_static_binding:
            setup = _classify_setup_command(unit)
            if setup is not None:
                steps.append(setup)
                continue

        scorer = _adapt_scoring_command(resolved_unit)
        if scorer is None:
            raise ReadinessContractError(
                "plan_unclassified_pre_scoring_command"
            )
        runner_family = scorer.runner_family
        scoring_command_sha256 = hashlib.sha256(unit.encode()).hexdigest()
        used_bindings.update(unit_used_bindings)
        if steps or fixtures or scorer.resolves_dependencies:
            if scorer.minimal_entrypoint is None:
                raise ReadinessError(
                    "verifier setup has no safe non-scoring entrypoint"
                )
            steps.append(
                DependencySetupStep(
                    (
                        "resolver_entrypoint"
                        if scorer.resolves_dependencies
                        else "minimal_entrypoint"
                    ),
                    scorer.minimal_entrypoint,
                )
            )
        scoring_boundary = True

    if bindings.keys() != used_bindings:
        raise ReadinessContractError("plan_static_binding_disallowed")
    if (
        not scoring_boundary
        or runner_family is None
        or scoring_command_sha256 is None
    ):
        raise ReadinessError(
            "official verifier has no statically proven scoring boundary"
        )
    return DependencySetupPlan(
        runner_family,
        tuple(steps),
        scoring_command_sha256,
        tuple(fixtures),
    )


def _logical_shell_units(script: str) -> list[str]:
    if re.search(r"<<-?\s*['\"]?[A-Za-z_]", script):
        raise ReadinessError("cannot safely parse verifier heredoc")
    physical = script.splitlines()
    logical: list[str] = []
    pending = ""
    for raw_line in physical:
        line = raw_line.strip()
        if not pending and (not line or line.startswith("#")):
            continue
        if line.endswith("\\"):
            pending += line[:-1].rstrip() + " "
            continue
        line = (pending + line).strip()
        pending = ""
        if line and not line.startswith("#"):
            logical.append(line)
    if pending:
        raise ReadinessError("cannot safely parse verifier line continuation")

    units: list[str] = []
    index = 0
    while index < len(logical):
        line = logical[index]
        if line.startswith("if "):
            block = [line]
            index += 1
            while index < len(logical) and logical[index] != "fi":
                if logical[index].startswith("if "):
                    raise ReadinessError(
                        "cannot safely parse nested verifier control flow"
                    )
                block.append(logical[index])
                index += 1
            if index >= len(logical):
                raise ReadinessError("unterminated verifier control flow")
            block.append("fi")
            units.append("\n".join(block))
        elif line in {"fi", "then", "else"} or re.match(
            r"^(?:for|while|until|case|select|function)\b", line
        ):
            raise ReadinessError("cannot safely parse verifier control flow")
        else:
            units.append(line)
        index += 1
    return units


def _shell_tokens(command: str) -> list[str]:
    try:
        return shlex.split(command, comments=True, posix=True)
    except ValueError as error:
        raise ReadinessError("cannot safely tokenize verifier command") from error


def _split_static_chain(command: str, separator: str) -> list[str]:
    parts: list[str] = []
    start = 0
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(command):
        character = command[index]
        if escaped:
            escaped = False
        elif character == "\\":
            escaped = True
        elif quote is not None:
            if character == quote:
                quote = None
        elif character in {"'", '"'}:
            quote = character
        elif command.startswith(separator, index):
            parts.append(command[start:index].strip())
            index += len(separator) - 1
            start = index + 1
        index += 1
    if quote is not None or escaped:
        raise ReadinessError("cannot safely parse verifier shell quoting")
    parts.append(command[start:].strip())
    return parts


def _package_segment(segment: str) -> bool:
    tokens = _validated_command_tokens(segment)
    if len(tokens) < 2 or tokens[0] not in {"apt-get", "apt", "apk", "dnf", "yum"}:
        return False
    manager = tokens.pop(0)
    while tokens and tokens[0].startswith("-"):
        option = tokens.pop(0)
        if option in {"-o", "--option", "-c", "--config-file"}:
            if not tokens:
                raise ReadinessError("package manager option is missing its value")
            tokens.pop(0)
    if not tokens:
        raise ReadinessError("package manager operation is unavailable")
    operations = {
        "apt-get": {"update", "install"},
        "apt": {"update", "install"},
        "apk": {"update", "add"},
        "dnf": {"makecache", "check-update", "install"},
        "yum": {"makecache", "check-update", "install"},
    }
    return tokens[0] in operations[manager]


def _build_segment(segment: str) -> bool:
    """Recognize a conservative, non-scoring native build invocation."""
    tokens = _validated_command_tokens(segment)
    if not tokens:
        return False
    executable = tokens[0]
    if executable == "./configure":
        return all("$" not in token for token in tokens[1:])
    if executable != "make":
        return False

    # A target could be an arbitrary test rule.  Admit static build targets but
    # never an explicitly test-shaped target; the scoring command remains the
    # only allowed test entrypoint in the readiness plan.
    targets: list[str] = []
    index = 1
    options_with_value = {"-C", "-f", "--directory", "--file"}
    while index < len(tokens):
        token = tokens[index]
        if token in options_with_value:
            if index + 1 >= len(tokens) or "$" in tokens[index + 1]:
                return False
            index += 2
            continue
        if token.startswith("--") or re.fullmatch(r"-[A-Za-z0-9]+", token):
            index += 1
            continue
        if "$" in token:
            return False
        if "=" in token:
            name, value = token.split("=", 1)
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) or not value:
                return False
            index += 1
            continue
        targets.append(token)
        index += 1
    return not any(
        re.search(r"(?:^|[-_])(test|check|verify|bench)(?:$|[-_])", target, re.I)
        for target in targets
    )


def _safe_relative_path(value: str) -> bool:
    path = PurePosixPath(value)
    return bool(value) and not path.is_absolute() and ".." not in path.parts and value not in {".", ".."}


def _filesystem_setup_segment(segment: str) -> bool:
    tokens = _validated_command_tokens(segment)
    if not tokens or any("$" in token for token in tokens):
        return False
    executable = tokens.pop(0)
    if executable == "rm":
        while tokens and tokens[0].startswith("-"):
            if not re.fullmatch(r"-(?:[rfRF]+)", tokens.pop(0)):
                return False
        return bool(tokens) and all(_safe_relative_path(token) for token in tokens)
    if executable == "cp":
        while tokens and tokens[0].startswith("-"):
            if not re.fullmatch(r"-(?:[rRaApP]+)", tokens.pop(0)):
                return False
        return len(tokens) == 2 and all(_safe_relative_path(token) for token in tokens)
    return False


def _directory_setup_segment(segment: str) -> bool:
    tokens = _validated_command_tokens(segment)
    return (
        len(tokens) == 2
        and tokens[0] == "cd"
        and "$" not in tokens[1]
        and (tokens[1] == ".." or _safe_relative_path(tokens[1]))
    )


def _git_setup_segment(segment: str) -> bool:
    tokens = _validated_command_tokens(segment)
    if len(tokens) < 2 or tokens[0] != "git" or any("$" in token for token in tokens):
        return False
    if tokens[1] == "clone":
        return (
            len(tokens) == 4
            and re.fullmatch(r"https://[^\s]+", tokens[2]) is not None
            and _safe_relative_path(tokens[3])
        )
    return (
        tokens[1] == "checkout"
        and len(tokens) == 3
        and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,255}", tokens[2]) is not None
    )


def _python_helper_setup_segment(segment: str) -> bool:
    """Recognize a static verifier-owned data/setup helper, never a scorer."""
    tokens = _validated_command_tokens(segment)
    if len(tokens) < 2 or tokens[0] not in {"python", "python3"}:
        return False
    script = PurePosixPath(tokens[1])
    tests_root = PurePosixPath("/tests")
    try:
        relative = script.relative_to(tests_root)
    except ValueError:
        return False
    stem = script.stem.casefold()
    if (
        script.suffix != ".py"
        or not relative.parts
        or ".." in relative.parts
        or re.search(r"(?:^|[_-])(test|score|verify|verifier)(?:$|[_-])", stem)
    ):
        return False
    return all("$" not in token for token in tokens[2:])


def _build_output_capture_pipeline(command: str) -> bool:
    parts = _split_static_chain(command, "|")
    if len(parts) != 2 or not _build_segment(parts[0]):
        return False
    consumer = _validated_command_tokens(parts[1])
    return len(consumer) == 2 and consumer[0] == "tee" and _safe_relative_path(consumer[1])


def _safe_shell_option_command(tokens: list[str]) -> bool:
    if len(tokens) < 2 or tokens[0] != "set":
        return False
    index = 1
    while index < len(tokens):
        option = tokens[index]
        if option == "-o":
            if index + 1 >= len(tokens) or tokens[index + 1] != "pipefail":
                return False
            index += 2
            continue
        if not option.startswith("-") or option == "-" or "x" in option[1:]:
            return False
        flags = option[1:]
        if any(flag not in {"e", "u", "o"} for flag in flags):
            return False
        if "o" in flags:
            if index + 1 >= len(tokens) or tokens[index + 1] != "pipefail":
                return False
            index += 1
        index += 1
    return True


def _validated_command_tokens(command: str) -> list[str]:
    tokens = _shell_tokens(command)
    is_export = bool(tokens and tokens[0] == "export")
    if is_export:
        candidates = tokens[1:]
        if not candidates:
            raise ReadinessError("bare verifier export is unavailable")
    else:
        candidates = []
        for token in tokens:
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", token) is None:
                break
            candidates.append(token)
    for candidate in candidates:
        assignment = re.fullmatch(
            r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)=(?P<value>.*)", candidate
        )
        if assignment is None:
            raise ReadinessError("verifier environment export is unavailable")
        name = assignment.group("name")
        value = assignment.group("value")
        if value not in SAFE_LITERAL_ASSIGNMENTS.get(name, set()):
            raise ReadinessError("verifier setup literal assignment is unavailable")
    remaining = list(tokens if is_export else tokens[len(candidates) :])
    if remaining and remaining[0] == "sudo":
        remaining.pop(0)
        while remaining and remaining[0].startswith("-"):
            remaining.pop(0)
        if remaining and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", remaining[0]):
            raise ReadinessError("verifier setup literal assignment is unavailable")
    return remaining


def _classify_setup_command(command: str) -> DependencySetupStep | None:
    if "$(" in command or "`" in command:
        raise ReadinessError("cannot safely parse verifier command substitution")
    if "||" in command:
        raise ReadinessError("cannot safely parse verifier fallback control flow")
    if _has_unquoted_setup_metacharacter(command):
        raise ReadinessError("cannot safely parse verifier shell metacharacter")
    if any(_credential_shaped_name(name) for name in _shell_variable_names(command)):
        raise ReadinessError("verifier setup contains credential-shaped data")

    chain_parts = _split_static_chain(command, "&&")
    if len(chain_parts) > 1:
        if all(_package_segment(part) for part in chain_parts):
            return DependencySetupStep("package_setup", command)
        # Preserve shell short-circuiting, but only when every atom is a
        # statically recognized setup action.  Sources and virtualenv creation
        # have identity checks that require their own probe boundaries.
        for part in chain_parts:
            if not part:
                raise ReadinessError("cannot safely parse verifier command chain")
            if _package_segment(part):
                continue
            if _build_segment(part):
                continue
            if (
                _filesystem_setup_segment(part)
                or _directory_setup_segment(part)
                or _git_setup_segment(part)
            ):
                continue
            raise ReadinessError("cannot safely parse mixed verifier command chain")
        return DependencySetupStep("compound_setup", command)

    if _package_segment(command):
        return DependencySetupStep("package_setup", command)
    if _build_segment(command):
        return DependencySetupStep("build_setup", command)
    if _filesystem_setup_segment(command):
        return DependencySetupStep("filesystem_setup", command)
    if _directory_setup_segment(command):
        return DependencySetupStep("environment", command)
    if _git_setup_segment(command):
        return DependencySetupStep("git_setup", command)
    if _python_helper_setup_segment(command):
        return DependencySetupStep("helper_setup", command)

    pipe_parts = _split_static_chain(command, "|")
    if len(pipe_parts) > 1:
        if _build_output_capture_pipeline(command):
            return DependencySetupStep("build_setup", command)
        if len(pipe_parts) != 2:
            raise ReadinessError("cannot safely parse verifier installer pipeline")
        producer = _validated_command_tokens(pipe_parts[0])
        consumer = _validated_command_tokens(pipe_parts[1])
        if (
            producer
            and producer[0] in {"curl", "wget"}
            and consumer
            and consumer[0] in {"sh", "bash"}
            and not any("$" in token for token in producer[1:])
        ):
            return DependencySetupStep("installer", command)
        raise ReadinessError("cannot safely parse verifier pipeline")

    tokens = _validated_command_tokens(command)
    if not tokens:
        return None
    executable = tokens[0]
    if executable in {"source", "."}:
        if len(tokens) != 2:
            raise ReadinessError("dynamic verifier environment source is unavailable")
        _parse_source_path_expression(tokens[1])
        return DependencySetupStep("environment_source", command)
    if executable == "set":
        if not _safe_shell_option_command(tokens):
            raise ReadinessError(
                "verifier shell tracing or positional set is unavailable"
            )
        return DependencySetupStep("environment", command)
    if executable in {"export", "cd", "mkdir", "chmod"}:
        return DependencySetupStep("environment", command)
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", executable) and len(tokens) == 1:
        return DependencySetupStep("environment", command)
    if executable in {"curl", "wget"}:
        if any("$" in token for token in tokens[1:]):
            raise ReadinessError("cannot safely parse dynamic verifier fetch")
        return DependencySetupStep("artifact_fetch", command)
    if (
        executable in {"pip", "pip3"}
        and len(tokens) >= 2
        and tokens[1]
        in {
            "install",
            "download",
            "wheel",
        }
    ):
        return DependencySetupStep("resolver", command)
    if (
        executable in {"python", "python3"}
        and len(tokens) >= 4
        and tokens[1:3] == ["-m", "pip"]
        and tokens[3] in {"install", "download", "wheel"}
    ):
        return DependencySetupStep("resolver", command)
    if executable == "uv" and len(tokens) >= 2:
        if tokens[1] == "venv":
            _parse_uv_venv_target(command)
            return DependencySetupStep("venv_create", command)
        if tokens[1] in {"sync", "lock"}:
            return DependencySetupStep("resolver", command)
        if len(tokens) >= 3 and tokens[1:3] == ["pip", "install"]:
            return DependencySetupStep("resolver", command)
    if (
        executable in {"npm", "pnpm", "yarn"}
        and len(tokens) >= 2
        and tokens[1]
        in {
            "ci",
            "install",
            "frozen-install",
        }
    ):
        return DependencySetupStep("resolver", command)
    return None


def _adapt_scoring_command(command: str) -> ScoringAdapter | None:
    tokens = _validated_command_tokens(command)
    if not tokens:
        return None
    executable = tokens[0]
    if executable in {"pytest", "py.test"}:
        return ScoringAdapter("pytest", shlex.join([executable, "--version"]))
    if (
        len(tokens) >= 3
        and executable in {"python", "python3"}
        and tokens[1:3] == ["-m", "pytest"]
    ):
        return ScoringAdapter("pytest", shlex.join([*tokens[:3], "--version"]))
    if executable == "uvx":
        scorer_index = _uv_scorer_index(tokens, 1)
        scorer = tokens[scorer_index]
        if scorer not in {"pytest", "py.test"}:
            raise ReadinessError("unsupported uvx verifier entrypoint")
        return ScoringAdapter(
            "pytest",
            shlex.join([*tokens[: scorer_index + 1], "--version"]),
            resolves_dependencies=True,
        )
    if len(tokens) >= 2 and tokens[:2] == ["uv", "run"]:
        scorer_index = _uv_scorer_index(tokens, 2)
        scorer = tokens[scorer_index]
        if scorer not in {"pytest", "py.test"}:
            raise ReadinessError("unsupported uv verifier entrypoint")
        return ScoringAdapter(
            "pytest",
            shlex.join([*tokens[: scorer_index + 1], "--version"]),
            resolves_dependencies=True,
        )
    if len(tokens) >= 2 and tokens[:2] == ["go", "test"]:
        return ScoringAdapter("go_test", RUNNER_MINIMAL_ENTRYPOINTS["go_test"])
    if len(tokens) >= 2 and tokens[:2] == ["cargo", "test"]:
        return ScoringAdapter("cargo_test", RUNNER_MINIMAL_ENTRYPOINTS["cargo_test"])
    if (
        len(tokens) >= 2
        and executable in {"npm", "pnpm", "yarn"}
        and (
            tokens[1] == "test" or (len(tokens) >= 3 and tokens[1:3] == ["run", "test"])
        )
    ):
        runner_family = f"{executable}_test"
        return ScoringAdapter(runner_family, RUNNER_MINIMAL_ENTRYPOINTS[runner_family])
    return None


def _uv_scorer_index(tokens: list[str], start: int) -> int:
    options_with_value = {
        "-p",
        "--python",
        "-w",
        "--with",
        "--from",
        "--index",
        "--default-index",
        "--index-url",
        "--extra-index-url",
        "--index-strategy",
        "--resolution",
        "--prerelease",
        "--python-platform",
    }
    flags = {
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "--isolated",
        "--no-cache",
        "--offline",
        "--refresh",
        "--no-progress",
        "--native-tls",
    }
    index = start
    while index < len(tokens) and tokens[index].startswith("-"):
        option = tokens[index]
        if "=" in option and option.split("=", 1)[0] in options_with_value:
            index += 1
        elif option in options_with_value:
            if index + 1 >= len(tokens):
                raise ReadinessError("uv verifier option is missing its value")
            index += 2
        elif option in flags:
            index += 1
        else:
            raise ReadinessError("unsupported uv verifier option")
    if index >= len(tokens):
        raise ReadinessError("uv verifier entrypoint is unavailable")
    return index


def _contains_dependency_intent(command: str) -> bool:
    return (
        re.search(
            r"(?<![A-Za-z0-9_-])(?:apt-get|apt|apk|dnf|yum|curl|wget|pip3?|uvx?|npm|pnpm|yarn)(?![A-Za-z0-9_-])",
            command,
        )
        is not None
    )


def _is_safe_environment_guard(block: str) -> bool:
    if (
        _contains_dependency_intent(block)
        or "$(" in block
        or "${" in block
        or "$[" in block
        or "`" in block
        or any(_credential_shaped_name(name) for name in _shell_variable_names(block))
    ):
        return False
    lines = block.splitlines()
    if len(lines) < 3 or lines[-1] != "fi":
        return False
    header = re.fullmatch(r"if\s+(.+?)\s*;\s*then", lines[0])
    if header is None:
        return False
    predicate = header.group(1)
    if _has_unquoted_guard_operator(predicate):
        return False
    predicate_tokens = _shell_tokens(predicate)
    if not (
        (
            len(predicate_tokens) >= 4
            and predicate_tokens[0] == "["
            and predicate_tokens[-1] == "]"
        )
        or (len(predicate_tokens) >= 3 and predicate_tokens[0] == "test")
    ):
        return False
    for line in lines[1:-1]:
        if "$(" in line or "`" in line or _has_unquoted_guard_operator(line):
            return False
        tokens = _shell_tokens(line)
        if not tokens:
            return False
        if tokens[0] == "echo" and "$" not in line and "`" not in line:
            continue
        if tokens[0] == "exit" and len(tokens) == 2 and tokens[1].isdigit():
            continue
        if tokens in (["true"], ["false"]):
            continue
        return False
    return True


def _parse_source_path_expression(expression: str) -> tuple[str, str]:
    normalized = expression.replace("${HOME}", "$HOME", 1)
    if normalized.startswith("$HOME/"):
        kind = "home"
        path = normalized.removeprefix("$HOME/")
    elif re.fullmatch(r"[A-Za-z0-9_./+-]+", normalized):
        kind = "static"
        path = normalized
    else:
        raise ReadinessError("dynamic verifier environment source is unavailable")
    if not path or ".." in PurePosixPath(path).parts or path.endswith("/"):
        raise ReadinessError("dynamic verifier environment source is unavailable")
    return kind, path


def _source_resolve_command(expression: str) -> str:
    kind, path = _parse_source_path_expression(expression)
    candidate = f'"$HOME"/{path}' if kind == "home" else shlex.quote(path)
    return f"test ! -L {candidate} && readlink -f -- {candidate}"


def _source_path_expression(command: str) -> str:
    tokens = _shell_tokens(command)
    if len(tokens) != 2 or tokens[0] not in {"source", "."}:
        raise ReadinessError("dynamic verifier environment source is unavailable")
    _parse_source_path_expression(tokens[1])
    return tokens[1]


def _normalized_source_expression(expression: str) -> str:
    return expression.replace("${HOME}", "$HOME", 1).rstrip("/")


def _parse_static_source_value(raw: str) -> str:
    if not raw:
        return ""
    if raw[0] in {"'", '"'}:
        if len(raw) < 2 or raw[-1] != raw[0]:
            raise ReadinessError("source assignment quoting is unavailable")
        if raw[0] == '"' and any(token in raw for token in ("$", "`", "\\")):
            raise ReadinessError("source assignment expansion is unavailable")
        return raw[1:-1]
    if re.fullmatch(r"[A-Za-z0-9_./:@%+,=-]*", raw) is None:
        raise ReadinessError("source assignment value is not static")
    return raw


def _credential_shaped_name(name: str) -> bool:
    words = {word for word in re.split(r"[^A-Za-z0-9]+", name.upper()) if word}
    normalized = re.sub(r"[^A-Za-z0-9]+", "", name).upper()
    return (
        any(
            marker in normalized
            for marker in (
                "SECRET",
                "TOKEN",
                "PASSWORD",
                "PASSWD",
                "CREDENTIAL",
                "AUTH",
                "APIKEY",
                "ACCESSKEY",
                "PRIVATEKEY",
                "CLIENTKEY",
            )
        )
        or "KEY" in words
        or normalized.endswith("KEY")
    )


def _shell_variable_names(command: str) -> set[str]:
    names = set(re.findall(r"(?<![A-Za-z0-9_])([A-Za-z_][A-Za-z0-9_]*)=", command))
    names.update(
        plain or braced
        for plain, braced in re.findall(
            r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z_][A-Za-z0-9_]*)[^}]*\})",
            command,
        )
    )
    export = re.match(r"^\s*export\s+([A-Za-z_][A-Za-z0-9_]*)\b", command)
    if export is not None:
        names.add(export.group(1))
    return names


def _source_stat_command(canonical_path: str) -> str:
    # `%f` is the locale-independent raw mode.  Do not use `%F` (localized
    # prose) or backslash separators: GNU stat prints `\t` literally.
    return f"LC_ALL=C stat -Lc '%d:%i:%s:%f' -- {shlex.quote(canonical_path)}"


def _parse_regular_file_identity(result: object) -> tuple[int, int, int]:
    fields = (result.stdout or "").strip().split(":")
    if (
        result.return_code != 0
        or len(fields) != 4
        or any(re.fullmatch(r"[0-9]+", field) is None for field in fields[:3])
        or re.fullmatch(r"[0-9a-fA-F]+", fields[3]) is None
        or not stat.S_ISREG(int(fields[3], 16))
    ):
        raise ReadinessError("file identity is not a regular file")
    device, inode, size = (int(field) for field in fields[:3])
    if (
        device < 0
        or inode <= 0
        or size < 0
        or any(value > (2**63 - 1) for value in (device, inode, size))
    ):
        raise ReadinessError("file identity is unavailable")
    return device, inode, size


def _parse_source_file_identity(result: object) -> tuple[int, int, int]:
    try:
        device, inode, size = _parse_regular_file_identity(result)
    except ReadinessError as error:
        raise ReadinessError("environment source is not a regular file") from error
    if size > MAX_SOURCE_BYTES:
        raise ReadinessError("environment source identity is unavailable")
    return device, inode, size


def _parse_source_content_digest(result: object) -> str:
    lines = (result.stdout or "").splitlines()
    if result.return_code != 0 or len(lines) != 1:
        raise ReadinessError("environment source digest is unavailable")
    digest = lines[0].split(maxsplit=1)[0]
    if re.fullmatch(r"[0-9a-f]{64}", digest) is None:
        raise ReadinessError("environment source digest is unavailable")
    return digest


def _parse_path_guard_source(
    lines: list[str], source_expression: str, canonical_path: str
) -> EnvironmentDelta | None:
    if len(lines) != 7 or lines[0] != 'case ":${PATH}:" in':
        return None
    match_pattern = re.fullmatch(
        r'\*:"(?P<path>(?:\$HOME|\$\{HOME\})/[A-Za-z0-9_./+-]+)":\*\)',
        lines[1],
    )
    match_export = re.fullmatch(
        r'export PATH="(?P<path>(?:\$HOME|\$\{HOME\})/[A-Za-z0-9_./+-]+):\$PATH"',
        lines[4],
    )
    if (
        match_pattern is None
        or match_export is None
        or lines[2] != ";;"
        or lines[3] != "*)"
        or lines[5:] != [";;", "esac"]
    ):
        return None
    path_expression = _normalized_source_expression(match_pattern.group("path"))
    if path_expression != _normalized_source_expression(match_export.group("path")):
        raise ReadinessError("source PATH guard is inconsistent")
    expected_parent = _normalized_source_expression(source_expression).rsplit("/", 1)[0]
    if path_expression != expected_parent:
        raise ReadinessError("source PATH guard escapes its canonical directory")
    canonical = PurePosixPath(canonical_path)
    if not canonical.is_absolute():
        raise ReadinessError("source canonical path is invalid")
    return EnvironmentDelta(path_prepend=(str(canonical.parent),))


def _parse_uv_venv_target(command: str) -> str:
    """Return the literal target of a safe ``uv venv`` command.

    The target is deliberately a typed fact rather than an arbitrary shell
    fragment.  In particular, ``--allow-existing`` is not accepted: a source
    file may only be bound to an environment created by this probe.
    """
    tokens = _shell_tokens(command)
    if len(tokens) < 2 or tokens[:2] != ["uv", "venv"]:
        raise ReadinessError("verifier virtualenv creator is unavailable")
    options_with_value = {
        "-p",
        "--python",
        "--python-preference",
        "--python-version",
        "--link-mode",
        "--prompt",
        "--directory",
        "--index-url",
        "--extra-index-url",
        "--find-links",
        "--default-index",
        "--index-strategy",
        "--keyring-provider",
        "--cache-dir",
        "--allow-insecure-host",
    }
    positional: list[str] = []
    index = 2
    while index < len(tokens):
        token = tokens[index]
        if token in {"--allow-existing", "--clear"}:
            raise ReadinessError("verifier virtualenv creator is unavailable")
        if token.startswith("-"):
            option = token.split("=", 1)[0]
            if option in options_with_value:
                if "=" not in token:
                    if index + 1 >= len(tokens):
                        raise ReadinessError(
                            "verifier virtualenv creator is unavailable"
                        )
                    index += 1
            elif token not in {
                "--seed",
                "--system-site-packages",
                "--relocatable",
                "--no-python-downloads",
                "--offline",
                "--native-tls",
                "--no-cache",
                "--managed-python",
                "--no-managed-python",
            }:
                raise ReadinessError("verifier virtualenv creator is unavailable")
        else:
            positional.append(token)
        index += 1
    if len(positional) != 1:
        raise ReadinessError("verifier virtualenv creator requires a literal target")
    target = positional[0]
    if (
        not target
        or any(character in target for character in ("$", "`", "\\"))
        or ".." in PurePosixPath(target).parts
        or not re.fullmatch(r"/?[A-Za-z0-9_./+~-]+", target)
        or target.endswith("/")
        or PurePosixPath(target) in {PurePosixPath("."), PurePosixPath("/")}
    ):
        raise ReadinessError("verifier virtualenv creator target is unavailable")
    return PurePosixPath(target).as_posix()


def _venv_source_matches_creator(
    source_expression: str, canonical_path: str, creator_target: str
) -> bool:
    """Check that a source is exactly ``<creator>/bin/activate``.

    Relative source paths are resolved by the verifier shell.  Comparing the
    canonical path suffix binds that resolution to the creator without
    guessing the container's working directory.
    """
    kind, path = _parse_source_path_expression(source_expression)
    if kind != "static":
        return False
    target = PurePosixPath(creator_target)
    expected = target / "bin" / "activate"
    canonical = PurePosixPath(canonical_path)
    if target.is_absolute():
        return canonical == expected
    return len(canonical.parts) >= len(expected.parts) and canonical.parts[
        -len(expected.parts) :
    ] == expected.parts


def _venv_activation_delta(
    content: bytes,
    source_expression: str,
    canonical_path: str,
    creator_target: str,
    creator_digest: str,
) -> EnvironmentDelta:
    """Derive only the safe PATH delta for a creator-bound activation file.

    The file is never sourced.  Its identity and digest were captured right
    after the creator command and are checked again by the caller before this
    adapter is reached; the textual checks below provide an additional guard
    against accidentally binding a non-activation file.
    """
    if not _venv_source_matches_creator(
        source_expression, canonical_path, creator_target
    ):
        raise ReadinessError("environment source is not creator-bound")
    if re.fullmatch(r"[0-9a-f]{64}", creator_digest) is None:
        raise ReadinessError("virtualenv activation identity is unavailable")
    if hashlib.sha256(content).hexdigest() != creator_digest:
        raise ReadinessError("virtualenv activation identity changed while reading")
    try:
        text = content.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ReadinessError("virtualenv activation is not UTF-8") from error
    root = PurePosixPath(canonical_path).parent.parent.as_posix()
    root_pattern = re.escape(root)
    if re.search(rf"(?m)^VIRTUAL_ENV=['\"]{root_pattern}['\"]$", text) is None:
        raise ReadinessError("virtualenv activation contract is unavailable")
    if re.search(r'(?m)^PATH="\$VIRTUAL_ENV/bin:\$PATH"$', text) is None:
        raise ReadinessError("virtualenv activation contract is unavailable")
    if not re.search(r"(?m)^export VIRTUAL_ENV$", text) or not re.search(
        r"(?m)^export PATH$", text
    ):
        raise ReadinessError("virtualenv activation contract is unavailable")
    return EnvironmentDelta(path_prepend=(f"{root}/bin",))


def _parse_environment_source(
    content: bytes,
    source_expression: str,
    canonical_path: str,
    *,
    creator_target: str | None = None,
    creator_digest: str | None = None,
) -> EnvironmentDelta:
    if len(content) > MAX_SOURCE_BYTES or b"\0" in content or b"\r" in content:
        raise ReadinessError("environment source content is unavailable")
    try:
        text = content.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ReadinessError("environment source is not UTF-8") from error
    lines = [
        line.strip()
        for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    path_guard = _parse_path_guard_source(lines, source_expression, canonical_path)
    if path_guard is not None:
        return path_guard
    if creator_target is not None and creator_digest is not None:
        return _venv_activation_delta(
            content,
            source_expression,
            canonical_path,
            creator_target,
            creator_digest,
        )

    assigned: set[str] = set()
    for line in lines:
        if any(token in line for token in ("$", "`", ";", "&", "|", "<", ">")):
            raise ReadinessError("environment source contains executable syntax")
        export = line.startswith("export ")
        body = line.removeprefix("export ") if export else line
        if export and "=" not in body:
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", body) is None:
                raise ReadinessError("environment source export is invalid")
            if body not in assigned:
                raise ReadinessError("environment source export has no assignment")
            if _credential_shaped_name(body):
                raise ReadinessError(
                    "environment source contains credential-shaped data"
                )
            continue
        assignment = re.fullmatch(
            r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)=(?P<value>.*)", body
        )
        if assignment is None:
            raise ReadinessError("environment source contains a command")
        name = assignment.group("name")
        if _credential_shaped_name(name):
            raise ReadinessError("environment source contains credential-shaped data")
        _parse_static_source_value(assignment.group("value"))
        assigned.add(name)
    if not lines:
        raise ReadinessError("environment source is empty")
    raise ReadinessError(
        "environment source requires a restricted non-sensitive PATH transform"
    )


def _render_environment_delta(delta: EnvironmentDelta) -> list[str]:
    return [f'export PATH={shlex.quote(path)}:"$PATH"' for path in delta.path_prepend]


def _render_dependency_setup_batch(
    plan: DependencySetupPlan,
    start: int,
    end: int,
    delta: EnvironmentDelta,
) -> str:
    if start < 0 or end > len(plan.steps) or start >= end:
        raise ReadinessError("cannot render an empty dependency setup batch")
    lines = ["set -e", *_render_environment_delta(delta)]
    for index in range(start, end):
        step = plan.steps[index]
        if step.kind in {"environment_source", "fixture_stage"}:
            raise ReadinessError("structured setup step must be separately applied")
        if step.kind == "venv_create":
            target = _parse_uv_venv_target(step.command)
            quoted_target = shlex.quote(target)
            activate = shlex.quote(f"{target}/bin/activate")
            config = shlex.quote(f"{target}/pyvenv.cfg")
            lines.extend(
                [
                    f"test ! -e {quoted_target} && test ! -L {quoted_target}",
                    step.command,
                    f"test -d {quoted_target} && test ! -L {quoted_target}",
                    f"test -f {activate} && test ! -L {activate}",
                    f"test -f {config} && test ! -L {config}",
                    f"printf '\\n{VENV_DIGEST_PREFIX}{index}\\n'",
                    f"sha256sum -- {activate}",
                ]
            )
        else:
            if _uses_non_strict_verifier_semantics(step):
                # Native builds and filesystem normalization can intentionally
                # fail in a debugging task.
                # The official verifier does not enable errexit, so preserve
                # that semantics for this proven non-scoring prefix while
                # recording the outcome.  Provisioning remains strict.
                lines.extend(
                    [
                        "set +e",
                        step.command,
                        "__astra_dependency_exit=$?",
                        "set -e",
                    ]
                )
            else:
                lines.append(step.command)
                lines.append("__astra_dependency_exit=0")
        lines.append(f"printf '\\n{STEP_RECEIPT_PREFIX}{index}=0\\n'")
        lines.append(
            f"printf '{STEP_EXIT_STATUS_PREFIX}{index}=%s\\n' \"$__astra_dependency_exit\""
        )
    return "\n".join(lines)


def _uses_non_strict_verifier_semantics(step: DependencySetupStep) -> bool:
    if step.kind in {"build_setup", "filesystem_setup"}:
        return True
    if step.kind != "compound_setup":
        return False
    parts = _split_static_chain(step.command, "&&")
    return bool(parts) and all(_build_segment(part) for part in parts)


def _has_unquoted_guard_operator(command: str) -> bool:
    quote: str | None = None
    escaped = False
    for character in command:
        if escaped:
            escaped = False
        elif character == "\\":
            escaped = True
        elif quote is not None:
            if character == quote:
                quote = None
        elif character in {"'", '"'}:
            quote = character
        elif character in {"|", "&", ";", "<", ">", "(", ")", "{", "}"}:
            return True
    return quote is not None or escaped


def _render_dependency_setup_command(plan: DependencySetupPlan) -> str:
    if not plan.steps:
        raise ReadinessError("cannot render an empty dependency setup plan")
    return _render_dependency_setup_batch(plan, 0, len(plan.steps), EnvironmentDelta())


def _has_unquoted_setup_metacharacter(command: str) -> bool:
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(command):
        character = command[index]
        if escaped:
            escaped = False
        elif character == "\\":
            escaped = True
        elif quote is not None:
            if character == quote:
                quote = None
        elif character in {"'", '"'}:
            quote = character
        elif character in {";", "<", ">"}:
            return True
        elif character == "&":
            if command.startswith("&&", index):
                index += 1
            else:
                return True
        index += 1
    if quote is not None or escaped:
        raise ReadinessError("cannot safely parse verifier shell quoting")
    return False


def _completed_setup_steps(stdout: str) -> list[int]:
    return [
        int(value)
        for value in re.findall(
            rf"(?m)^{re.escape(STEP_RECEIPT_PREFIX)}([0-9]+)=0$", stdout
        )
    ]


def _completed_setup_exit_codes(stdout: str) -> dict[int, int]:
    records = re.findall(
        rf"(?m)^{re.escape(STEP_EXIT_STATUS_PREFIX)}([0-9]+)=([0-9]+)$", stdout
    )
    values = {int(index): int(status) for index, status in records}
    if len(values) != len(records):
        raise ReadinessError("verifier dependency setup exit receipts are duplicated")
    return values


async def probe_task_readiness(
    task_path: Path,
    projection: dict[str, str],
    session_suffix: str,
    state_dir: Path,
    image_materialization_semaphore: asyncio.Semaphore | None = None,
) -> dict[str, object]:
    task = Task(task_path)
    if task.has_steps:
        raise ReadinessError("verifier readiness requires one task per round")
    verifier_mode = resolve_task_verifier_mode(task.config)
    verifier_env_config = resolve_effective_verifier_env_config(task.config, None)
    task_env_config = verifier_env_config or task.config.environment
    test_path = task.paths.discovered_test_path_for(task_env_config.os)
    if test_path is None:
        raise ReadinessError(f"task has no official verifier: {task_path}")
    if task.config.verifier.env:
        raise ReadinessError(
            "task verifier.env must be empty; only the exact launcher projection is allowed"
        )
    dependency_setup_timeout_seconds = task.config.verifier.timeout_sec
    dependency_setup_timeout_seconds = _validated_timeout_seconds(
        dependency_setup_timeout_seconds, field="task verifier timeout_sec"
    )
    image_reference = task_env_config.docker_image
    if not image_reference:
        raise ReadinessError(
            "verifier readiness requires a declared prebuilt Docker image"
        )
    build_timeout_seconds = _validated_timeout_seconds(
        task_env_config.build_timeout_sec,
        field="task environment build_timeout_sec",
    )
    healthcheck_timeout_seconds = (
        0.0
        if verifier_env_config is not None
        else _healthcheck_timeout_bound(
            getattr(task_env_config, "healthcheck", None)
        )
    )
    tail_timeout_seconds = _task_tail_timeout_bound(
        build_timeout_seconds=build_timeout_seconds,
        dependency_setup_timeout_seconds=dependency_setup_timeout_seconds,
        healthcheck_timeout_seconds=healthcheck_timeout_seconds,
    )
    image_id, repo_digests, image_source = await _inspect_image(
        image_reference,
        pull_timeout_seconds=build_timeout_seconds,
        materialization_semaphore=image_materialization_semaphore,
    )
    if not repo_digests:
        raise ReadinessError(
            "verifier readiness image has no immutable repository digest"
        )
    # The readiness clone must execute the same immutable bytes recorded in its
    # receipt; never allow a mutable tag to change between inspect and start.
    task_env_config.docker_image = repo_digests[0]
    trial_agent = TrialAgentConfig(extra_allowed_hosts=["10.222.1.10"])
    trial_environment = TrialEnvironmentConfig(
        type=EnvironmentType.DOCKER,
        force_build=False,
        delete=True,
        extra_allowed_hosts=["10.222.1.10"],
    )
    plan = resolve_trial_network_plan(
        task.config,
        trial_agent,
        trial_environment,
        None,
        verifier_mode=verifier_mode,
        env_config=verifier_env_config,
    )
    baseline = plan.verifier_env_baseline or plan.agent_env_baseline
    build_context = (
        task.paths.tests_dir if verifier_env_config else task.paths.environment_dir
    )
    trial_root = state_dir / "verifier-readiness" / f"trial-{session_suffix}"
    trial_root.parent.mkdir(mode=0o700, exist_ok=True)
    trial_paths = TrialPaths(trial_root)
    trial_paths.mkdir()
    trial_paths.chmod_dir()
    environment_paths = EnvironmentPaths.for_os(task_env_config.os)
    mounts = [
        ServiceVolumeConfig(
            type="bind",
            source=trial_paths.verifier_dir.resolve().absolute().as_posix(),
            target=str(environment_paths.verifier_dir),
        )
    ]
    environment = EnvironmentFactory.create_environment_from_config(
        config=trial_environment,
        environment_dir=build_context,
        environment_name=task.short_name,
        session_id=f"astra-readiness-{session_suffix}",
        trial_paths=trial_paths,
        task_env_config=task_env_config,
        mounts=mounts,
        network_policy=baseline,
        phase_network_policies=[plan.verifier_phase],
    )
    _install_cancellation_safe_compose_collector(environment)
    _install_compose_registration(environment, state_dir)
    environment.default_user = task.config.verifier.user
    started = False
    terminal_boundary_reached = False
    record: dict[str, object] | None = None
    primary_error: BaseException | None = None
    tail_deadline = asyncio.get_running_loop().time() + tail_timeout_seconds
    try:
        # The official scoring command is intentionally not run here. It runs
        # once in the scored Harbor trial after the agent has acted.
        # Treat a partial start as started for cleanup purposes: Docker may
        # have created a project before Harbor reports an error.
        started = True
        await _await_tail_stage(
            lambda: environment.start(force_build=False),
            stage="environment start",
            timeout_seconds=build_timeout_seconds,
            tail_deadline=tail_deadline,
        )
        if verifier_env_config is None and healthcheck_timeout_seconds:
            await _await_tail_stage(
                environment.run_healthcheck,
                stage="environment healthcheck",
                timeout_seconds=healthcheck_timeout_seconds,
                tail_deadline=tail_deadline,
            )
        async with _verifier_phase(
            environment,
            baseline,
            plan.verifier_phase,
            tail_deadline=tail_deadline,
        ):
            dependency_setup_probe = await _await_tail_stage(
                lambda: _probe_verifier_container(
                        environment,
                        projection,
                        environment_paths,
                        task.paths.tests_dir,
                        test_path,
                        verifier_env_config is not None,
                        dependency_setup_timeout_seconds,
                    ),
                stage="dependency setup probe",
                timeout_seconds=dependency_setup_timeout_seconds,
                tail_deadline=tail_deadline,
            )

        terminal_boundary_reached = True
        record = {
            "schema": SCHEMA,
            "task_sha256": task_tree_sha256(task_path),
            "environment_id": environment.environment_id,
            "image_id": image_id,
            "repo_digests": repo_digests,
            "image_source": image_source,
            "environment_lifecycle": "started_deleted",
            "verifier_env_sha256": canonical_json_sha256(projection),
            "verifier_env_keys": sorted(projection),
            "official_verifier": {
                "test_sha256": hashlib.sha256(test_path.read_bytes()).hexdigest(),
                "execution_mode": "container_lifecycle_non_scoring",
                "terminal_boundary_reached": terminal_boundary_reached,
                "score_eligible": False,
                "reward_disposition": "scored_trial_only",
                "environment_deleted": True,
            },
            "dependency_setup_probe": dependency_setup_probe,
        }
    except BaseException as error:
        primary_error = error
    cleanup_error: BaseException | None = None
    try:
        if started:
            await _await_cleanup_owner(
                environment, tail_deadline=tail_deadline
            )
    except BaseException as error:
        cleanup_error = error
    finally:
        shutil.rmtree(trial_root, ignore_errors=True)
    if primary_error is not None:
        if cleanup_error is not None:
            primary_error.add_note(
                f"verifier readiness cleanup also failed: {cleanup_error}"
            )
        raise primary_error.with_traceback(primary_error.__traceback__)
    if cleanup_error is not None:
        if isinstance(cleanup_error, asyncio.CancelledError):
            raise cleanup_error
        if isinstance(cleanup_error, ReadinessStageError):
            raise cleanup_error
        raise ReadinessStageError(
            "cleanup", "exception", _static_exception_category(cleanup_error)
        ) from cleanup_error
    if record is None:
        raise ReadinessError("verifier readiness probe did not produce a receipt")
    return record


def _flatten_task_probe_error(
    *, task_index: int, task_name: str, error: BaseException
) -> TaskProbeStageError:
    if isinstance(error, ReadinessStageError):
        stage = error.stage
        kind = error.kind
        category = error.category
        subcategory = error.subcategory
    elif isinstance(error, ImageMaterializationError):
        stage = error.stage
        kind = error.kind
        category = "image_materialization"
        subcategory = None
    elif isinstance(error, DependencyProbeError):
        stage = "dependency setup probe"
        kind = "exception"
        category = "readiness_contract"
        subcategory = error.subcategory
    else:
        stage = "task probe"
        kind = "exception"
        category = "internal"
        subcategory = None
    return TaskProbeStageError(
        task_index=task_index,
        task_name=task_name,
        stage=stage,
        kind=kind,
        category=category,
        subcategory=subcategory,
    )


async def run(
    config: Path,
    ledger: Path,
    state_dir: Path,
    *,
    max_concurrency: int = DEFAULT_MAX_CONCURRENCY,
) -> None:
    if type(max_concurrency) is not int or not 1 <= max_concurrency <= MAX_CONCURRENCY:
        raise ReadinessError(
            f"verifier readiness concurrency must be between 1 and {MAX_CONCURRENCY}"
        )
    payload = json.loads(config.read_text(encoding="utf-8"))
    verifier = payload.get("verifier")
    projection = verifier.get("env") if isinstance(verifier, dict) else None
    if (
        not isinstance(projection, dict)
        or set(projection) != PROJECTION_KEYS
        or not all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in projection.items()
        )
    ):
        raise ReadinessError("final verifier network projection is not exact")
    tasks = payload.get("tasks")
    if not isinstance(tasks, list) or len(tasks) < 3:
        raise ReadinessError(
            "verifier readiness requires at least three selected tasks"
        )
    paths: list[Path] = []
    task_names: list[str] = []
    for index, entry in enumerate(tasks):
        if not isinstance(entry, dict) or set(entry) != {"path"}:
            raise ReadinessError(f"task {index} is not a closed local path")
        path = Path(os.path.abspath(Path(entry["path"]).expanduser()))
        if not path.is_dir():
            raise ReadinessError(f"task {index} path is unavailable: {path}")
        if TASK_NAME_PATTERN.fullmatch(path.name) is None:
            raise ReadinessError(f"task {index} basename is invalid")
        paths.append(path)
        task_names.append(path.name)

    # This exercises the daemon's *configured* primary registry route before a
    # task-specific image pull can consume its full official build timeout.
    # It is not a task image or verifier mutation and never enters scoring.
    await _probe_primary_registry_transport()

    queue: asyncio.Queue[tuple[int, Path]] = asyncio.Queue()
    for item in enumerate(paths):
        queue.put_nowait(item)
    records: list[dict[str, object] | None] = [None] * len(paths)
    failures: list[tuple[int, str, Exception] | None] = [None] * len(paths)
    admission_closed = False
    image_materialization_semaphore = asyncio.Semaphore(
        IMAGE_MATERIALIZATION_CONCURRENCY
    )

    async def worker() -> None:
        nonlocal admission_closed
        while True:
            if admission_closed:
                return
            try:
                index, path = queue.get_nowait()
            except asyncio.QueueEmpty:
                return
            try:
                records[index] = await probe_task_readiness(
                    path,
                    projection,
                    f"{os.getpid()}-{index}",
                    state_dir,
                    image_materialization_semaphore,
                )
            except Exception as error:
                failures[index] = (index, task_names[index], error)
                admission_closed = True
            finally:
                queue.task_done()

    workers = [
        asyncio.create_task(worker()) for _ in range(min(max_concurrency, len(paths)))
    ]
    await asyncio.gather(*workers)
    for failure in failures:
        if failure is not None:
            task_index, task_name, error = failure
            raise _flatten_task_probe_error(
                task_index=task_index,
                task_name=task_name,
                error=error,
            ) from error
    if any(record is None for record in records):
        raise ReadinessError("verifier readiness worker lost a task result")
    output = {
        "schema": "astra.harness.verifier_readiness_ledger.v1",
        "config_sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
        "projection_sha256": canonical_json_sha256(projection),
        "records": records,
    }
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(ledger, flags, 0o400)
    try:
        serialized = (json.dumps(output, indent=2, sort_keys=True) + "\n").encode()
        os.write(descriptor, serialized)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--ledger", type=Path, required=True)
    parser.add_argument("--domain-state", type=Path, required=True)
    parser.add_argument(
        "--max-concurrency",
        type=int,
        default=DEFAULT_MAX_CONCURRENCY,
        choices=range(1, MAX_CONCURRENCY + 1),
    )
    args = parser.parse_args()
    try:
        config = Path(os.path.abspath(args.config.expanduser()))
        state_dir = args.domain_state.resolve(strict=True)
        asyncio.run(
            run(
                config,
                args.ledger.resolve(),
                state_dir,
                max_concurrency=args.max_concurrency,
            )
        )
        print(json.dumps({"ok": True, "ledger": str(args.ledger)}, sort_keys=True))
        return 0
    except TaskProbeStageError as error:
        print(f"astra harness: verifier readiness failed: {error}", file=sys.stderr)
        return 78
    except (ReadinessStageError, ImageMaterializationError) as error:
        print(f"astra harness: verifier readiness failed: {error}", file=sys.stderr)
        return 78
    except (OSError, ValueError, TypeError, json.JSONDecodeError, ReadinessError):
        print(
            "astra harness: verifier readiness failed: readiness_error",
            file=sys.stderr,
        )
        return 78
    except BaseException:
        print(
            "astra harness: verifier readiness failed: internal_error",
            file=sys.stderr,
        )
        return 78


if __name__ == "__main__":
    raise SystemExit(main())
