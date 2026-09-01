#!/usr/bin/env python3
"""Read-only preflight for a fresh Terminal-Bench/Harbor trial."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

EXPECTED_BENCHMARK_AGENT = "harbor_adapter:Astra"
# The default is deliberately explicit for ordinary scored runs.  The closed
# config also records its selector, however, so a registered high-thinking
# model can be used for an auditable comparison without changing the runner.
DEFAULT_BENCHMARK_SELECTOR = "deepseek-v4-flash(thinking:high)"
BUILD_INFO_SCHEMA = "astra.build_info.v1"
BUILD_INFO_KEYS = {"schema", "git_sha", "git_dirty", "target", "profile"}
BENCHMARK_PROVENANCE_ENV_KEYS = {
    "ASTRA_EXPECTED_BUILD_GIT_SHA",
    "ASTRA_HARNESS_BINARY_SHA256",
    "ASTRA_HARNESS_BUILD_PROFILE",
    "ASTRA_HARNESS_TASK_SET_SHA256",
    "ASTRA_HARBOR_HTTP_PROXY",
    "ASTRA_HARBOR_HTTPS_PROXY",
}
SCORED_SOURCE_CONFIG_KEYS = {
    "jobs_dir",
    "n_attempts",
    "install_only",
    "timeout_multiplier",
    "agent_timeout_multiplier",
    "verifier_timeout_multiplier",
    "agent_setup_timeout_multiplier",
    "environment_build_timeout_multiplier",
    "debug",
    "quiet",
    "n_concurrent_trials",
    "retry",
    "environment",
    "verifier",
    "metrics",
    "agents",
    "datasets",
    "tasks",
    "artifacts",
    "extra_instruction_paths",
    "source_jobs",
}
SAFE_CHILD_ENV_KEYS = (
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "PYTHONPATH",
    "DOCKER_HOST",
    "XDG_RUNTIME_DIR",
)
VERIFIER_NETWORK_ENV_KEYS = (
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
)
VERIFIER_NO_PROXY = "localhost,127.0.0.1,::1,172.17.0.1"
VERIFIER_READINESS_TASK_CONCURRENCY = 4
VERIFIER_READINESS_IMAGE_MATERIALIZATION_CONCURRENCY = 1
VERIFIER_READINESS_IMAGE_INSPECT_TIMEOUT_SECONDS = 15.0
VERIFIER_READINESS_MAX_IMAGE_INSPECTIONS_PER_MATERIALIZATION = 3
VERIFIER_READINESS_PROCESS_TERMINATION_SECONDS = 4.0
VERIFIER_READINESS_NETWORK_TRANSITION_SECONDS = 60.0
VERIFIER_READINESS_NETWORK_TRANSITIONS_PER_PROBE = 2
VERIFIER_READINESS_TAIL_PROCESS_TERMINATION_SECONDS = 4.0
VERIFIER_READINESS_CLEANUP_GRACE_SECONDS = 64.0
VERIFIER_READINESS_DEFAULT_BUILD_TIMEOUT_SECONDS = 600.0
DEPENDENCY_SETUP_POLICY = "astra.harness.dependency_setup_entrypoint.v3"
MAX_SOURCE_BYTES = 64 * 1024
RUNNER_MINIMAL_ENTRYPOINTS = {
    "go_test": "go version",
    "cargo_test": "cargo --version",
    "npm_test": "npm list --depth=0",
    "pnpm_test": "pnpm list --depth=0",
    "yarn_test": "yarn list --depth=0",
}


def check(name: str, ok: bool, detail: str, required: bool = True) -> dict[str, Any]:
    return {"name": name, "ok": bool(ok), "required": required, "detail": detail}


def child_environment(extra_env: dict[str, str] | None = None) -> dict[str, str]:
    """Build a probe environment without inheriting credentials by default."""
    environment = {
        key: value
        for key in SAFE_CHILD_ENV_KEYS
        if (value := os.environ.get(key)) is not None
    }
    environment.update({"NO_PROXY": "*", "no_proxy": "*"})
    if extra_env:
        environment.update(extra_env)
    return environment


def run_text(argv: list[str], timeout: float = 5.0) -> tuple[int, str, str]:
    try:
        completed = subprocess.run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=child_environment(),
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return 127, "", str(error)
    return completed.returncode, completed.stdout.strip(), completed.stderr.strip()


def run_text_with_env(
    argv: list[str], extra_env: dict[str, str], timeout: float = 5.0
) -> tuple[int, str, str]:
    """Run a probe without ever printing its environment or secrets."""
    try:
        completed = subprocess.run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=child_environment(extra_env),
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return 127, "", str(error)
    return completed.returncode, completed.stdout.strip(), completed.stderr.strip()


def _validated_proxy_url(environment: dict[str, str], *names: str) -> str:
    configured = {
        value.strip()
        for name in names
        if (value := environment.get(name)) is not None and value.strip()
    }
    if len(configured) != 1:
        raise ValueError(
            f"{names[0]}/{names[1]} must resolve to one non-empty proxy URL"
        )
    value = configured.pop()
    parsed = urllib.parse.urlsplit(value)
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
    ):
        raise ValueError(
            f"{names[0]}/{names[1]} must be an http(s) proxy URL without credentials, path, query, or fragment"
        )
    return value.removesuffix("/")


def verifier_network_projection(
    environment: dict[str, str] | None = None,
) -> dict[str, str]:
    """Return the only network environment allowed in a scored verifier.

    Provider and database credentials are intentionally outside this allowlist.
    Ambient bypass entries are also ignored: the verifier receives only the
    local Docker/host routes required by the harness contract.
    """
    environment = os.environ if environment is None else environment
    proxy_names = ("HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy")
    configured = [environment.get(name, "").strip() for name in proxy_names]
    if not any(configured):
        http_proxy = ""
        https_proxy = ""
    else:
        http_proxy = _validated_proxy_url(environment, "HTTP_PROXY", "http_proxy")
        https_proxy = _validated_proxy_url(environment, "HTTPS_PROXY", "https_proxy")
    return {
        "HTTP_PROXY": http_proxy,
        "HTTPS_PROXY": https_proxy,
        "http_proxy": http_proxy,
        "https_proxy": https_proxy,
        "NO_PROXY": VERIFIER_NO_PROXY,
        "no_proxy": VERIFIER_NO_PROXY,
    }


def resolve_model_selector(selector: str) -> tuple[str, str | None]:
    """Mirror Astra's trailing thinking-suffix grammar for a harness gate."""
    selector = selector.strip()
    if not selector.endswith(")") or "(" not in selector:
        return selector, None
    base, marker = selector.rsplit("(", 1)
    marker = marker[:-1]
    mode: str | None = "thinking" if marker == "thinking" else None
    if marker.startswith("thinking:"):
        value = marker.removeprefix("thinking:")
        mode = value if value in {"low", "medium", "high", "max"} else None
        if value.startswith("budget:"):
            budget = value.removeprefix("budget:")
            mode = value if budget.isdigit() and int(budget) <= 2**32 - 1 else None
    return (base.rstrip(), mode) if mode is not None else (selector, None)


def configured_model_requirements(
    config: Path,
) -> tuple[bool, list[tuple[str, str]], str]:
    try:
        payload = json.loads(config.read_text(encoding="utf-8"))
        agents = payload.get("agents")
        if not isinstance(agents, list) or len(agents) != 1:
            raise ValueError("agents must contain exactly one entry")
        requirements: list[tuple[str, str]] = []
        for index, agent in enumerate(agents):
            name = agent.get("name") if isinstance(agent, dict) else None
            if name != EXPECTED_BENCHMARK_AGENT:
                raise ValueError(
                    f"agent {index} must be the exact Astra adapter "
                    f"{EXPECTED_BENCHMARK_AGENT!r}"
                )
            selector = agent.get("model_name") if isinstance(agent, dict) else None
            if not isinstance(selector, str) or not selector.strip():
                raise ValueError(f"agent {index} has no model_name")
            selector = selector.strip()
            base, thinking = resolve_model_selector(selector)
            if not base:
                raise ValueError(f"agent {index} has an empty base model name")
            if thinking not in {None, "high"}:
                raise ValueError(
                    f"agent {index} must use no thinking suffix or thinking:high"
                )
            requirement = (base.casefold(), thinking)
            if requirement not in requirements:
                requirements.append(requirement)
        return (
            True,
            requirements,
            json.dumps(
                {"selected_base_models": [name for name, _ in requirements]},
                ensure_ascii=False,
            ),
        )
    except (OSError, ValueError, TypeError) as error:
        return False, [], f"invalid configured model selection: {error}"


def _require_exact_keys(value: Any, keys: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be an object")
    actual = set(value)
    if actual != keys:
        missing = sorted(keys - actual)
        unexpected = sorted(actual - keys)
        raise ValueError(
            f"{label} keys differ from the scored-run contract: "
            f"missing={missing}, unexpected={unexpected}"
        )
    return value


def _validate_benchmark_config_payload(
    payload: Any,
    expected_verifier: dict[str, Any],
    expected_jobs_dir: Path | None,
) -> None:
    payload = _require_exact_keys(
        payload,
        SCORED_SOURCE_CONFIG_KEYS,
        "benchmark config",
    )
    jobs_dir = payload["jobs_dir"]
    if not isinstance(jobs_dir, str) or not jobs_dir.strip():
        raise ValueError("jobs_dir must be a non-empty path")
    if (
        expected_jobs_dir is not None
        and Path(jobs_dir).expanduser().resolve() != expected_jobs_dir.resolve()
    ):
        raise ValueError("jobs_dir does not match the launcher-owned result directory")
    fixed_values = {
        "n_attempts": 1,
        "install_only": False,
        "timeout_multiplier": 1.0,
        "agent_timeout_multiplier": None,
        "verifier_timeout_multiplier": None,
        "agent_setup_timeout_multiplier": None,
        "environment_build_timeout_multiplier": None,
        "debug": False,
        "quiet": False,
        "n_concurrent_trials": 1,
        "metrics": [],
        "datasets": [],
        "artifacts": [],
        "extra_instruction_paths": [],
        "source_jobs": [],
    }
    for key, expected in fixed_values.items():
        if payload[key] != expected or type(payload[key]) is not type(expected):
            raise ValueError(f"{key} must be exactly {expected!r}")
    if payload["retry"] != {"max_retries": 0}:
        raise ValueError("retry must be exactly {'max_retries': 0}")
    if payload["environment"] != {
        "type": "docker",
        "force_build": False,
        "delete": True,
    }:
        raise ValueError(
            "environment must be exact Docker defaults with deletion enabled"
        )
    if payload["verifier"] != expected_verifier:
        raise ValueError("verifier differs from the exact scored-run contract")

    agents = payload["agents"]
    if not isinstance(agents, list) or len(agents) != 1:
        raise ValueError("agents must contain exactly one entry")
    agent = _require_exact_keys(agents[0], {"name", "model_name", "env"}, "agent 0")
    if agent["name"] != EXPECTED_BENCHMARK_AGENT:
        raise ValueError("agent 0 has a non-canonical adapter")
    selector = agent["model_name"]
    if not isinstance(selector, str) or not selector.strip():
        raise ValueError("agent 0 has no model selector")
    _, thinking = resolve_model_selector(selector.strip())
    if thinking not in {None, "high"}:
        raise ValueError("agent 0 must use no thinking suffix or thinking:high")
    environment = _require_exact_keys(
        agent["env"], BENCHMARK_PROVENANCE_ENV_KEYS, "agent 0 env"
    )
    if not all(isinstance(value, str) for value in environment.values()):
        raise ValueError("agent provenance env values must be strings")
    for key in ("ASTRA_HARBOR_HTTP_PROXY", "ASTRA_HARBOR_HTTPS_PROXY"):
        if environment[key] != f"${{{key}}}":
            raise ValueError(
                f"agent 0 env {key} must be the exact host-resolved placeholder"
            )
    if environment["ASTRA_HARNESS_BUILD_PROFILE"] != "debug":
        raise ValueError("scored runs require the debug build profile")

    tasks = payload["tasks"]
    if not isinstance(tasks, list) or len(tasks) < 3:
        raise ValueError("tasks must contain at least three entries")
    for index, task in enumerate(tasks):
        task = _require_exact_keys(task, {"path"}, f"task {index}")
        if not isinstance(task["path"], str) or not task["path"].strip():
            raise ValueError(f"task {index} path must be non-empty")


def validate_benchmark_source_config(
    config: Path, expected_jobs_dir: Path | None = None
) -> tuple[bool, str]:
    """Validate a closed scored-run schema before Harbor resolves defaults.

    Harbor's Pydantic models intentionally ignore some unknown or deprecated
    fields.  A scored run cannot inherit that extensibility: every field that
    can change task inputs, retries, timeouts, execution, or verification is
    either fixed here or rejected.
    """
    try:
        _validate_benchmark_config_payload(
            json.loads(config.read_text(encoding="utf-8")),
            {"disable": False},
            expected_jobs_dir,
        )
        return True, "closed scored-run source config"
    except (OSError, ValueError, TypeError, json.JSONDecodeError) as error:
        return False, f"invalid scored-run source config: {error}"


def validate_benchmark_finalized_config(
    config: Path,
    expected_verifier_network: dict[str, str],
    expected_jobs_dir: Path | None = None,
) -> tuple[bool, str]:
    """Validate the launcher's sole permitted mutation of a closed source plan."""
    try:
        if set(expected_verifier_network) != set(VERIFIER_NETWORK_ENV_KEYS):
            raise ValueError("expected verifier projection has non-canonical keys")
        if (
            verifier_network_projection(expected_verifier_network)
            != expected_verifier_network
        ):
            raise ValueError("expected verifier projection has non-canonical values")
        _validate_benchmark_config_payload(
            json.loads(config.read_text(encoding="utf-8")),
            {"disable": False, "env": expected_verifier_network},
            expected_jobs_dir,
        )
        return True, "closed scored-run config with exact verifier network projection"
    except (OSError, ValueError, TypeError, json.JSONDecodeError) as error:
        return False, f"invalid finalized scored-run config: {error}"


def validate_build_info(
    raw: str,
    *,
    expected_git_sha: str,
    expected_target: str | None,
    expected_profile: str,
) -> tuple[bool, str]:
    try:
        value = _require_exact_keys(json.loads(raw), BUILD_INFO_KEYS, "build info")
        expected = {
            "schema": BUILD_INFO_SCHEMA,
            "git_sha": expected_git_sha,
            "git_dirty": False,
            "profile": expected_profile,
        }
        for key, wanted in expected.items():
            if value.get(key) != wanted:
                raise ValueError(
                    f"build info {key}={value.get(key)!r}, expected {wanted!r}"
                )
        target = value.get("target")
        if not isinstance(target, str) or not target.strip():
            raise ValueError("build info target is missing")
        if expected_target is not None and target != expected_target:
            raise ValueError(
                f"build info target={target!r}, expected {expected_target!r}"
            )
        return True, json.dumps(value, sort_keys=True)
    except (ValueError, TypeError, json.JSONDecodeError) as error:
        return False, f"invalid build info: {error}"


def probe_build_info(
    binary: Path,
    *,
    expected_git_sha: str,
    expected_target: str | None,
    expected_profile: str = "debug",
) -> tuple[bool, str]:
    rc, out, err = run_text([str(binary), "--build-info-json"], timeout=10.0)
    if rc != 0:
        return False, f"build-info probe failed: {err or 'non-zero exit'}"
    return validate_build_info(
        out,
        expected_git_sha=expected_git_sha,
        expected_target=expected_target,
        expected_profile=expected_profile,
    )


def benchmark_task_tree_sha256(path: Path) -> str:
    task_digest = hashlib.sha256()
    for entry in sorted(
        path.rglob("*"), key=lambda value: value.relative_to(path).as_posix()
    ):
        relative = entry.relative_to(path).as_posix()
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
            raise ValueError(f"unsupported task tree entry: {entry}")
        mode = entry.stat(follow_symlinks=False).st_mode & 0o7777
        task_digest.update(
            kind
            + b"\0"
            + relative.encode()
            + b"\0"
            + f"{mode:o}".encode()
            + b"\0"
            + content
        )
    return task_digest.hexdigest()


def benchmark_task_set_sha256(paths: list[Path]) -> tuple[str, list[dict[str, str]]]:
    """Hash the exact task trees that Harbor will resolve."""
    combined = hashlib.sha256()
    tasks: list[dict[str, str]] = []
    # The task-set identity must survive sealing into a different parent
    # directory.  Task roots are content-addressed and their basenames are
    # required to be unique by the closed-plan validation, whereas absolute
    # paths deliberately change between the official cache and a snapshot.
    for path in sorted(paths, key=lambda value: value.name):
        digest = benchmark_task_tree_sha256(path)
        tasks.append({"path": str(path), "sha256": digest})
        combined.update(path.name.encode() + b"\0" + digest.encode() + b"\0")
    return combined.hexdigest(), tasks


def validate_benchmark_tasks(
    config: Path,
    expected_verifier_network: dict[str, str] | None = None,
) -> tuple[bool, str]:
    """Require an immutable three-trial diagnostic plan."""
    try:
        if expected_verifier_network is None:
            shape_ok, shape_detail = validate_benchmark_source_config(config)
        else:
            shape_ok, shape_detail = validate_benchmark_finalized_config(
                config, expected_verifier_network
            )
        if not shape_ok:
            raise ValueError(shape_detail)
        payload = json.loads(config.read_text(encoding="utf-8"))
        if payload.get("n_attempts") != 1:
            raise ValueError("n_attempts must be explicitly set to 1")
        if payload.get("datasets") != []:
            raise ValueError("datasets must be explicitly empty")
        if payload.get("source_jobs", []) != []:
            raise ValueError("source_jobs must be empty")
        if payload.get("install_only", False) is not False:
            raise ValueError("install_only must be false")
        verifier = payload.get("verifier")
        if not isinstance(verifier, dict) or verifier.get("disable") is not False:
            raise ValueError("verifier.disable must be explicitly false")
        tasks = payload.get("tasks")
        if not isinstance(tasks, list) or len(tasks) < 3:
            raise ValueError("tasks must contain at least three entries")
        paths: list[Path] = []
        timeouts: list[int] = []
        for index, task in enumerate(tasks):
            raw = task.get("path") if isinstance(task, dict) else None
            if not isinstance(raw, str) or not raw.strip():
                raise ValueError(f"task {index} has no path")
            path = lexical_absolute(Path(raw))
            if not path.is_dir():
                raise ValueError(f"task {index} path is unavailable: {path}")
            task_file = path / "task.toml"
            with task_file.open("rb") as stream:
                timeout = tomllib.load(stream)["agent"]["timeout_sec"]
            seconds = float(timeout)
            if seconds <= 0 or not seconds.is_integer():
                raise ValueError(f"{task_file} has invalid agent.timeout_sec")
            paths.append(path)
            timeouts.append(int(seconds))
        if len(set(paths)) != len(paths):
            raise ValueError("task paths must be unique")
        task_set_sha256, task_digests = benchmark_task_set_sha256(paths)
        agents = payload.get("agents")
        if (
            not isinstance(agents, list)
            or len(agents) != 1
            or not isinstance(agents[0], dict)
        ):
            raise ValueError("task provenance requires exactly one agent entry")
        environment = agents[0].get("env")
        expected_task_set = (
            environment.get("ASTRA_HARNESS_TASK_SET_SHA256")
            if isinstance(environment, dict)
            else None
        )
        if expected_task_set != task_set_sha256:
            raise ValueError(
                "ASTRA_HARNESS_TASK_SET_SHA256 does not match the selected task trees"
            )
        return True, json.dumps(
            {
                "task_count": len(paths),
                "agent_count": 1,
                "attempt_count": 1,
                "resulting_trial_count": len(paths),
                "verification_enabled": True,
                "agent_timeout_seconds": dict(
                    zip((path.name for path in paths), timeouts, strict=True)
                ),
                "task_set_sha256": task_set_sha256,
                "tasks": task_digests,
            },
            ensure_ascii=False,
        )
    except (OSError, KeyError, ValueError, TypeError, tomllib.TOMLDecodeError) as error:
        return False, f"invalid benchmark task set: {error}"


def _catalog_page_url(url: str, cursor: dict[str, str] | None) -> str:
    if cursor is None:
        return url
    parsed = urllib.parse.urlsplit(url)
    query = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True)
    query.extend(
        [
            ("after_provider", cursor["provider"]),
            ("after_name", cursor["model_name"]),
            ("after_offering_id", cursor["model_id"]),
        ]
    )
    return urllib.parse.urlunsplit(parsed._replace(query=urllib.parse.urlencode(query)))


def fetch_model_catalog(
    url: str,
    token: str,
    requirements: list[tuple[str, str]],
    timeout: float = 8.0,
) -> tuple[bool, str]:
    """Drain and validate exact selected routes without putting the token in argv."""
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    cursor: dict[str, str] | None = None
    seen_cursors: set[tuple[str, str, str]] = set()
    items: list[dict[str, Any]] = []
    expected_total: int | None = None
    expected_revision: str | None = None
    try:
        while True:
            request = urllib.request.Request(
                _catalog_page_url(url, cursor),
                headers={"Authorization": f"Bearer {token}"},
            )
            with opener.open(request, timeout=timeout) as response:
                body = response.read(2 * 1024 * 1024)
            page = json.loads(body.decode("utf-8"))
            page_items = page.get("items") if isinstance(page, dict) else None
            total = page.get("total") if isinstance(page, dict) else None
            revision = page.get("catalog_revision") if isinstance(page, dict) else None
            limit = page.get("limit") if isinstance(page, dict) else None
            if not isinstance(page_items, list) or not all(
                isinstance(item, dict) for item in page_items
            ):
                raise ValueError("items must be a list of objects")
            if not isinstance(total, int) or total < 0:
                raise ValueError("total must be a non-negative integer")
            if not isinstance(limit, int) or not 1 <= limit <= 200:
                raise ValueError("limit must be between 1 and 200")
            if not isinstance(revision, str) or not revision:
                raise ValueError("catalog_revision is missing")
            if expected_total is None:
                expected_total = total
                expected_revision = revision
            elif total != expected_total or revision != expected_revision:
                raise ValueError("catalog changed during pagination")
            items.extend(page_items)
            next_cursor = page.get("next_cursor")
            if next_cursor is None:
                break
            if not page_items or not isinstance(next_cursor, dict):
                raise ValueError("continuation cursor requires a non-empty page")
            next_values = tuple(
                next_cursor.get(key) for key in ("provider", "model_name", "model_id")
            )
            if not all(isinstance(value, str) and value for value in next_values):
                raise ValueError("continuation cursor is malformed")
            cursor_key = (next_values[0], next_values[1], next_values[2])
            if cursor_key in seen_cursors:
                raise ValueError("catalog continuation cursor repeated")
            seen_cursors.add(cursor_key)
            cursor = {
                "provider": cursor_key[0],
                "model_name": cursor_key[1],
                "model_id": cursor_key[2],
            }
        if len(items) != expected_total:
            raise ValueError(
                f"catalog ended with {len(items)} items but advertised {expected_total}"
            )

        selected: list[dict[str, Any]] = []
        failures: list[str] = []
        for base, thinking_mode in requirements:
            matches = [
                item
                for item in items
                if isinstance(item.get("name"), str) and item["name"].casefold() == base
            ]
            active = [item for item in matches if item.get("is_active") is True]
            if len(active) != 1:
                failures.append(
                    f"{base}: expected one active exact Offering, found {len(active)}"
                )
                continue
            capability = active[0].get("thinking_capability")
            if thinking_mode == "high" and capability not in {"both", "effort_only"}:
                failures.append(
                    f"{base}: requested controllable thinking but capability is {capability!r}"
                )
                continue
            selected.append(
                {
                    "name": active[0].get("name"),
                    "offering_id": active[0].get("offering_id"),
                    "thinking_capability": capability,
                }
            )
        detail = {
            "models_url": url,
            "catalog_total": expected_total,
            "catalog_revision": expected_revision,
            "selected": selected,
            "failures": failures,
        }
        return not failures, json.dumps(detail, ensure_ascii=False)
    except (
        OSError,
        urllib.error.URLError,
        urllib.error.HTTPError,
        UnicodeDecodeError,
        ValueError,
        TypeError,
    ) as error:
        return False, f"invalid model catalog response: {error}"


def sha256(path: Path) -> str | None:
    try:
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
        return digest.hexdigest()
    except OSError:
        return None


def lexical_absolute(path: Path) -> Path:
    """Preserve a /proc/<owner>/fd path instead of resolving its held root."""
    return Path(os.path.abspath(path.expanduser()))


def canonical_json_sha256(value: Any) -> str:
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def official_verifier_sha256(task_path: Path) -> str:
    tests = task_path / "tests"
    candidates = [
        path for path in (tests / "test.sh", tests / "test.bat") if path.is_file()
    ]
    if len(candidates) != 1:
        raise ValueError("official verifier identity is unavailable or ambiguous")
    digest = sha256(candidates[0])
    if digest is None:
        raise ValueError("official verifier identity cannot be read")
    return digest


def _effective_verifier_environment(
    task: dict[str, Any],
) -> tuple[dict[str, Any], bool]:
    environment = task.get("environment")
    verifier = task.get("verifier")
    if not isinstance(environment, dict) or not isinstance(verifier, dict):
        raise ValueError("verifier readiness task budget is unavailable")
    verifier_environment = verifier.get("environment")
    if verifier_environment is not None and not isinstance(
        verifier_environment, dict
    ):
        raise ValueError("verifier readiness verifier.environment is invalid")
    mode = verifier.get("environment_mode")
    if mode is not None and mode not in {"shared", "separate"}:
        raise ValueError("verifier readiness environment_mode is invalid")
    if mode == "shared" and verifier_environment is not None:
        raise ValueError(
            "verifier readiness shared mode cannot define verifier.environment"
        )
    separate = mode == "separate" or (
        mode is None and verifier_environment is not None
    )
    if separate and verifier_environment is not None:
        return verifier_environment, True
    return environment, separate


def _healthcheck_timeout_bound(healthcheck: object | None) -> float:
    if healthcheck is None:
        return 0.0
    if not isinstance(healthcheck, dict):
        raise ValueError("verifier readiness healthcheck is invalid")

    def duration(field: str, default: float, *, positive: bool = False) -> float:
        value = healthcheck.get(field, default)
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value < 0
            or (positive and value <= 0)
        ):
            raise ValueError(f"verifier readiness healthcheck {field} is invalid")
        return float(value)

    timeout = duration("timeout_sec", 30.0, positive=True)
    interval = duration("interval_sec", 5.0)
    start_period = duration("start_period_sec", 0.0)
    start_interval = duration("start_interval_sec", 5.0)
    retries = healthcheck.get("retries", 3)
    if isinstance(retries, bool) or not isinstance(retries, int) or retries <= 0:
        raise ValueError("verifier readiness healthcheck retries is invalid")
    bound = (
        start_period
        + timeout
        + start_interval
        + retries * timeout
        + (retries - 1) * interval
    )
    if not math.isfinite(bound):
        raise ValueError("verifier readiness healthcheck timeout bound is invalid")
    return bound


def verifier_readiness_timeout(
    config: Path,
    *,
    max_concurrency: int = VERIFIER_READINESS_TASK_CONCURRENCY,
) -> float:
    """Conservatively bound the serialized-image/concurrent-probe schedule."""
    if not isinstance(max_concurrency, int) or isinstance(max_concurrency, bool):
        raise ValueError("verifier readiness concurrency is invalid")
    if max_concurrency <= 0:
        raise ValueError("verifier readiness concurrency is invalid")
    payload = json.loads(config.read_text(encoding="utf-8"))
    entries = payload.get("tasks")
    if not isinstance(entries, list) or not entries:
        raise ValueError("verifier readiness config has no tasks")
    materialization_budgets: list[float] = []
    tail_budgets: list[float] = []
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
            raise ValueError("verifier readiness task path is invalid")
        task_toml = lexical_absolute(Path(entry["path"])) / "task.toml"
        task = tomllib.loads(task_toml.read_text(encoding="utf-8"))
        verifier = task.get("verifier")
        if not isinstance(verifier, dict):
            raise ValueError("verifier readiness task budget is unavailable")
        environment, separate_verifier = _effective_verifier_environment(task)
        build = environment.get(
            "build_timeout_sec",
            VERIFIER_READINESS_DEFAULT_BUILD_TIMEOUT_SECONDS,
        )
        verifier_timeout = verifier.get("timeout_sec", 600.0)
        if (
            isinstance(build, bool)
            or not isinstance(build, (int, float))
            or isinstance(verifier_timeout, bool)
            or not isinstance(verifier_timeout, (int, float))
        ):
            raise ValueError("verifier readiness task timeout is unavailable")
        if (
            not math.isfinite(build)
            or not math.isfinite(verifier_timeout)
            or build <= 0
            or verifier_timeout <= 0
        ):
            raise ValueError("verifier readiness task timeout is invalid")
        # Image materialization is admitted one-at-a-time. A digest-pinned
        # cache miss can consume cache-inspect, cache-reinspect, pull, and
        # post-pull-inspect; the pull itself uses the task's build budget.
        materialization_budgets.append(
            float(build)
            + VERIFIER_READINESS_MAX_IMAGE_INSPECTIONS_PER_MATERIALIZATION
            * VERIFIER_READINESS_IMAGE_INSPECT_TIMEOUT_SECONDS
            + VERIFIER_READINESS_PROCESS_TERMINATION_SECONDS
        )
        # Once materialized, a worker can spend a second build budget starting
        # the immutable image, the verifier setup budget, and cleanup grace.
        tail_budgets.append(
            float(build)
            + float(verifier_timeout)
            + (
                0.0
                if separate_verifier
                else _healthcheck_timeout_bound(environment.get("healthcheck"))
            )
            + VERIFIER_READINESS_NETWORK_TRANSITIONS_PER_PROBE
            * VERIFIER_READINESS_NETWORK_TRANSITION_SECONDS
            + VERIFIER_READINESS_CLEANUP_GRACE_SECONDS
            + VERIFIER_READINESS_TAIL_PROCESS_TERMINATION_SECONDS
        )
    concurrency = min(max_concurrency, len(tail_budgets))
    serial_materialization = math.fsum(materialization_budgets)
    tail_work = math.fsum(tail_budgets)
    largest_tail = max(tail_budgets)
    # Before the final image is materialized, every interval where the image
    # lane is idle has all workers occupied by tails. Afterward at most one
    # largest tail remains on the critical path. This standard list-scheduling
    # bound preserves concurrency without pretending all tails are serial:
    #   work / C + (1 - 1/C) * largest_job
    parallel_tail_bound = tail_work / concurrency + (
        1.0 - 1.0 / concurrency
    ) * largest_tail
    return serial_materialization + parallel_tail_bound


def validate_verifier_readiness_record(
    record: dict[str, Any],
    *,
    expected_task_sha256: str,
    expected_test_sha256: str,
    expected_projection: dict[str, str],
    expected_dependency_setup_seconds: float,
) -> tuple[bool, str]:
    try:
        value = _require_exact_keys(
            record,
            {
                "schema",
                "task_sha256",
                "environment_id",
                "image_id",
                "repo_digests",
                "image_source",
                "environment_lifecycle",
                "verifier_env_sha256",
                "verifier_env_keys",
                "official_verifier",
                "dependency_setup_probe",
            },
            "verifier readiness record",
        )
        if value["schema"] != "astra.harness.verifier_readiness.v6":
            raise ValueError("verifier readiness schema is not canonical")
        if value["task_sha256"] != expected_task_sha256:
            raise ValueError("verifier readiness task digest changed")
        if not re.fullmatch(
            r"(?:[0-9a-f]{32}|[0-9a-f]{64})", str(value["environment_id"])
        ):
            raise ValueError("verifier readiness environment identity is invalid")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(value["image_id"])):
            raise ValueError(
                "verifier readiness image identity is not content-addressed"
            )
        repo_digests = value["repo_digests"]
        if (
            not isinstance(repo_digests, list)
            or not repo_digests
            or not all(
                isinstance(item, str) and re.search(r"@sha256:[0-9a-f]{64}$", item)
                for item in repo_digests
            )
        ):
            raise ValueError(
                "verifier readiness repo digests are not content-addressed"
            )
        if value["image_source"] not in {"pulled", "digest-pinned-cache"}:
            raise ValueError("verifier readiness image source is not authoritative")
        if value["environment_lifecycle"] != "started_deleted":
            raise ValueError("verifier readiness environment lifecycle is invalid")
        expected_keys = sorted(VERIFIER_NETWORK_ENV_KEYS)
        if value["verifier_env_keys"] != expected_keys:
            raise ValueError(
                "verifier readiness environment contains non-canonical keys"
            )
        if value["verifier_env_sha256"] != canonical_json_sha256(expected_projection):
            raise ValueError("verifier readiness environment projection changed")
        official = _require_exact_keys(
            value["official_verifier"],
            {
                "test_sha256",
                "execution_mode",
                "terminal_boundary_reached",
                "score_eligible",
                "reward_disposition",
                "environment_deleted",
            },
            "official verifier preflight",
        )
        if official["test_sha256"] != expected_test_sha256:
            raise ValueError("official verifier test identity changed")
        if official["execution_mode"] != "container_lifecycle_non_scoring":
            raise ValueError("verifier readiness executed a scoring command")
        if official["terminal_boundary_reached"] is not True:
            raise ValueError(
                "official verifier readiness did not reach a terminal boundary"
            )
        if (
            official["score_eligible"] is not False
            or official["reward_disposition"] != "scored_trial_only"
        ):
            raise ValueError(
                "verifier readiness must leave scoring to the official trial"
            )
        if official["environment_deleted"] is not True:
            raise ValueError("verifier readiness environment was not deleted")
        dependency = _require_exact_keys(
            value["dependency_setup_probe"],
            {
                "mode",
                "plan",
                "plan_sha256",
                "budget_seconds",
                "invocations",
                "batches",
                "batches_sha256",
                "sources",
                "sources_sha256",
                "fixtures",
                "fixtures_sha256",
                "executions",
                "scoring_invoked",
            },
            "verifier dependency setup probe",
        )
        if dependency["budget_seconds"] != expected_dependency_setup_seconds:
            raise ValueError("verifier dependency setup probe budget is invalid")
        plan = _require_exact_keys(
            dependency["plan"],
            {
                "policy",
                "shell",
                "runner_family",
                "rendered_command_sha256",
                "scoring_command_sha256",
                "fixtures",
                "steps",
            },
            "verifier dependency setup plan",
        )
        if plan["policy"] != DEPENDENCY_SETUP_POLICY:
            raise ValueError("verifier dependency setup policy is invalid")
        if plan["shell"] != "bash":
            raise ValueError("verifier dependency setup shell is invalid")
        if plan["runner_family"] not in {
            "pytest",
            "go_test",
            "cargo_test",
            "npm_test",
            "pnpm_test",
            "yarn_test",
        }:
            raise ValueError("verifier scoring family is not canonical")
        if (
            not isinstance(plan["scoring_command_sha256"], str)
            or re.fullmatch(r"[0-9a-f]{64}", plan["scoring_command_sha256"]) is None
        ):
            raise ValueError("verifier scoring command identity is invalid")
        steps = plan["steps"]
        if not isinstance(steps, list):
            raise ValueError("verifier dependency setup steps are invalid")
        allowed_step_kinds = {
            "package_setup",
            "installer",
            "artifact_fetch",
            "build_setup",
            "compound_setup",
            "filesystem_setup",
            "git_setup",
            "helper_setup",
            "fixture_stage",
            "environment",
            "environment_guard",
            "environment_source",
            "venv_create",
            "resolver",
            "resolver_entrypoint",
            "minimal_entrypoint",
        }
        normalized_steps = []
        for step in steps:
            normalized = _require_exact_keys(
                step, {"kind", "command_sha256"}, "verifier dependency setup step"
            )
            if normalized["kind"] not in allowed_step_kinds:
                raise ValueError("verifier dependency setup step kind is invalid")
            if (
                not isinstance(normalized["command_sha256"], str)
                or re.fullmatch(r"[0-9a-f]{64}", normalized["command_sha256"]) is None
            ):
                raise ValueError(
                    "verifier dependency setup command identity is invalid"
                )
            normalized_steps.append(normalized)
        plan_fixtures = plan["fixtures"]
        if not isinstance(plan_fixtures, list):
            raise ValueError("verifier fixture plan is invalid")
        normalized_plan_fixtures = []
        for fixture in plan_fixtures:
            normalized = _require_exact_keys(
                fixture,
                {
                    "sequence",
                    "step_index",
                    "source_relative_sha256",
                    "source_path_sha256",
                    "basename_sha256",
                    "source_sha256",
                },
                "verifier fixture plan",
            )
            if (
                type(normalized["sequence"]) is not int
                or normalized["sequence"] < 0
                or type(normalized["step_index"]) is not int
                or not 0 <= normalized["step_index"] < len(normalized_steps)
                or normalized_steps[normalized["step_index"]]["kind"]
                != "fixture_stage"
                or any(
                    not isinstance(normalized[key], str)
                    or re.fullmatch(r"[0-9a-f]{64}", normalized[key]) is None
                    for key in (
                        "source_relative_sha256",
                        "source_path_sha256",
                        "basename_sha256",
                        "source_sha256",
                    )
                )
            ):
                raise ValueError("verifier fixture plan is invalid")
            normalized_plan_fixtures.append(normalized)
        if len({item["step_index"] for item in normalized_plan_fixtures}) != len(
            normalized_plan_fixtures
        ):
            raise ValueError("verifier fixture plan has duplicate step bindings")
        if dependency["plan_sha256"] != canonical_json_sha256(plan):
            raise ValueError("verifier dependency setup plan identity changed")
        source_indexes = [
            index
            for index, step in enumerate(normalized_steps)
            if step["kind"] == "environment_source"
        ]
        fixture_indexes = [
            index
            for index, step in enumerate(normalized_steps)
            if step["kind"] == "fixture_stage"
        ]
        if fixture_indexes != sorted(
            item["step_index"] for item in normalized_plan_fixtures
        ):
            raise ValueError("verifier fixture step binding changed")
        if dependency["mode"] == "no_setup":
            if normalized_steps or normalized_plan_fixtures:
                raise ValueError("no-setup verifier contains dependency steps")
            if plan["rendered_command_sha256"] is not None:
                raise ValueError("no-setup verifier contains a rendered command")
        elif dependency["mode"] == "executed":
            if not normalized_steps and not normalized_plan_fixtures:
                raise ValueError("executed verifier dependency setup is empty")
            if source_indexes or fixture_indexes:
                if plan["rendered_command_sha256"] is not None:
                    raise ValueError("source-bound setup has a monolithic command")
            elif (
                not isinstance(plan["rendered_command_sha256"], str)
                or re.fullmatch(r"[0-9a-f]{64}", plan["rendered_command_sha256"])
                is None
            ):
                raise ValueError("rendered dependency setup identity is invalid")
        else:
            raise ValueError("verifier dependency setup mode is invalid")
        if normalized_steps and normalized_steps[-1]["kind"] not in {
            "minimal_entrypoint",
            "resolver_entrypoint",
        }:
            raise ValueError("verifier setup has no final non-scoring entrypoint")
        static_entrypoint = RUNNER_MINIMAL_ENTRYPOINTS.get(plan["runner_family"])
        if (
            normalized_steps
            and normalized_steps[-1]["kind"] == "minimal_entrypoint"
            and static_entrypoint is not None
            and normalized_steps[-1]["command_sha256"]
            != hashlib.sha256(static_entrypoint.encode()).hexdigest()
        ):
            raise ValueError("verifier runner entrypoint policy changed")
        batches = dependency["batches"]
        if not isinstance(batches, list):
            raise ValueError("verifier dependency setup batches are invalid")
        normalized_batches = []
        for batch in batches:
            normalized = _require_exact_keys(
                batch,
                {"start", "end", "command_sha256", "step_exit_codes"},
                "verifier dependency setup batch",
            )
            if (
                type(normalized["start"]) is not int
                or type(normalized["end"]) is not int
                or not 0 <= normalized["start"] < normalized["end"] <= len(normalized_steps)
                or not isinstance(normalized["step_exit_codes"], list)
                or len(normalized["step_exit_codes"])
                != normalized["end"] - normalized["start"]
                or any(
                    type(code) is not int or not 0 <= code <= 255
                    for code in normalized["step_exit_codes"]
                )
                or not isinstance(normalized["command_sha256"], str)
                or re.fullmatch(r"[0-9a-f]{64}", normalized["command_sha256"]) is None
            ):
                raise ValueError("verifier dependency setup batch is invalid")
            for offset, code in enumerate(normalized["step_exit_codes"]):
                kind = normalized_steps[normalized["start"] + offset]["kind"]
                if code != 0 and kind not in {
                    "build_setup",
                    "compound_setup",
                    "filesystem_setup",
                }:
                    raise ValueError(
                        "strict verifier dependency setup step failed"
                    )
            normalized_batches.append(normalized)
        if dependency["batches_sha256"] != canonical_json_sha256(batches):
            raise ValueError("verifier dependency setup batch identity changed")
        sources = dependency["sources"]
        if not isinstance(sources, list) or len(sources) != len(source_indexes):
            raise ValueError("verifier environment source cardinality changed")
        normalized_sources = []
        for expected_index, source in zip(source_indexes, sources, strict=True):
            normalized = _require_exact_keys(
                source,
                {
                    "step_index",
                    "canonical_path",
                    "device",
                    "inode",
                    "content_sha256",
                    "content_bytes",
                    "environment_delta_sha256",
                    "resolve_command_sha256",
                    "stat_command_sha256",
                    "digest_command_sha256",
                },
                "verifier environment source binding",
            )
            digests = (
                normalized["content_sha256"],
                normalized["environment_delta_sha256"],
                normalized["resolve_command_sha256"],
                normalized["stat_command_sha256"],
                normalized["digest_command_sha256"],
            )
            canonical_path = normalized["canonical_path"]
            if (
                normalized["step_index"] != expected_index
                or type(normalized["device"]) is not int
                or normalized["device"] < 0
                or type(normalized["inode"]) is not int
                or normalized["inode"] <= 0
                or type(normalized["content_bytes"]) is not int
                or not 0 <= normalized["content_bytes"] <= MAX_SOURCE_BYTES
                or not isinstance(canonical_path, str)
                or not canonical_path.startswith("/")
                or ".." in Path(canonical_path).parts
                or any(
                    not isinstance(digest, str)
                    or re.fullmatch(r"[0-9a-f]{64}", digest) is None
                    for digest in digests
                )
            ):
                raise ValueError("verifier environment source binding is invalid")
            normalized_sources.append(normalized)
        if dependency["sources_sha256"] != canonical_json_sha256(sources):
            raise ValueError("verifier environment source identity changed")

        fixtures = dependency["fixtures"]
        if (
            not isinstance(fixtures, list)
            or len(fixtures) != len(normalized_plan_fixtures)
        ):
            raise ValueError("verifier fixture cardinality changed")
        normalized_fixtures = []
        for expected, fixture in zip(
            normalized_plan_fixtures, fixtures, strict=True
        ):
            normalized = _require_exact_keys(
                fixture,
                {
                    "sequence",
                    "step_index",
                    "cwd_sha256",
                    "source_sha256",
                    "destination_sha256",
                    "content_sha256",
                    "content_bytes",
                    "destination_probe_command_sha256",
                    "stat_command_sha256",
                    "digest_command_sha256",
                },
                "verifier fixture binding",
            )
            digest_keys = (
                "cwd_sha256",
                "source_sha256",
                "destination_sha256",
                "content_sha256",
                "destination_probe_command_sha256",
                "stat_command_sha256",
                "digest_command_sha256",
            )
            if (
                normalized["sequence"] != expected["sequence"]
                or normalized["step_index"] != expected["step_index"]
                or normalized["source_sha256"] != expected["source_path_sha256"]
                or normalized["content_sha256"] != expected["source_sha256"]
                or type(normalized["content_bytes"]) is not int
                or normalized["content_bytes"] < 0
                or any(
                    not isinstance(normalized[key], str)
                    or re.fullmatch(r"[0-9a-f]{64}", normalized[key]) is None
                    for key in digest_keys
                )
            ):
                raise ValueError("verifier fixture binding is invalid")
            normalized_fixtures.append(normalized)
        if dependency["fixtures_sha256"] != canonical_json_sha256(fixtures):
            raise ValueError("verifier fixture identity changed")

        expected_operations: list[tuple[str, dict[str, Any] | None]] = []
        batch_cursor = 0
        source_cursor = 0
        fixture_cursor = 0
        fixture_workdir_observed = False
        special_indexes = set(source_indexes) | set(fixture_indexes)
        index = 0
        while index < len(normalized_steps):
            if index in fixture_indexes:
                fixture = normalized_fixtures[fixture_cursor]
                if not fixture_workdir_observed:
                    expected_operations.append(("fixture_workdir_probe", None))
                    fixture_workdir_observed = True
                expected_operations.append(("fixture_destination_probe", fixture))
                expected_operations.append(("fixture_stat", fixture))
                expected_operations.append(("fixture_digest", fixture))
                fixture_cursor += 1
                index += 1
                continue
            if index in source_indexes:
                source = normalized_sources[source_cursor]
                expected_operations.append(("source_resolve", source))
                expected_operations.append(("source_stat_before", source))
                expected_operations.append(("source_digest_before", source))
                expected_operations.append(("source_stat_after", source))
                expected_operations.append(("source_digest_after", source))
                source_cursor += 1
                index += 1
                continue
            end = index + 1
            while end < len(normalized_steps) and end not in special_indexes:
                end += 1
            if batch_cursor >= len(normalized_batches):
                raise ValueError("verifier dependency setup batch is missing")
            batch = normalized_batches[batch_cursor]
            if batch["start"] != index or batch["end"] != end:
                raise ValueError("verifier dependency setup coverage changed")
            expected_operations.append(("dependency_setup", batch))
            batch_cursor += 1
            index = end
        if batch_cursor != len(normalized_batches):
            raise ValueError("verifier dependency setup batch is extraneous")
        if not special_indexes and normalized_batches:
            if (
                normalized_batches[0]["command_sha256"]
                != plan["rendered_command_sha256"]
            ):
                raise ValueError("rendered dependency setup identity changed")
        invocations = dependency["invocations"]
        if not isinstance(invocations, list):
            raise ValueError("verifier environment invocations are invalid")
        normalized_invocations = []
        for invocation in invocations:
            observed = _require_exact_keys(
                invocation,
                {"sequence", "kind", "command_sha256", "exit_code"},
                "verifier environment invocation",
            )
            if (
                type(observed["sequence"]) is not int
                or type(observed["exit_code"]) is not int
                or observed["kind"]
                not in {
                    "readability_probe",
                    "fixture_workdir_probe",
                    "fixture_destination_probe",
                    "fixture_stat",
                    "fixture_digest",
                    "dependency_setup",
                    "source_resolve",
                    "source_stat_before",
                    "source_stat_after",
                    "source_digest_before",
                    "source_digest_after",
                    "scoring",
                }
                or not isinstance(observed["command_sha256"], str)
                or re.fullmatch(r"[0-9a-f]{64}", observed["command_sha256"]) is None
            ):
                raise ValueError("verifier environment invocation is invalid")
            normalized_invocations.append(observed)
        observed_scoring = any(
            invocation["kind"] == "scoring"
            or invocation["command_sha256"] == plan["scoring_command_sha256"]
            for invocation in normalized_invocations
        )
        if (
            not isinstance(dependency["scoring_invoked"], bool)
            or dependency["scoring_invoked"] != observed_scoring
        ):
            raise ValueError("verifier scoring observation is not authoritative")
        if observed_scoring:
            raise ValueError("verifier readiness invoked scoring")
        expected_invocations: list[tuple[str, dict[str, Any] | None]] = [
            ("readability_probe", None),
            *expected_operations,
        ]
        if len(normalized_invocations) != len(expected_invocations):
            raise ValueError("verifier environment invocation cardinality changed")
        for sequence, (invocation, expected) in enumerate(
            zip(normalized_invocations, expected_invocations, strict=True)
        ):
            expected_kind, operation = expected
            if (
                invocation["sequence"] != sequence
                or invocation["kind"] != expected_kind
                or invocation["exit_code"] != 0
            ):
                raise ValueError("verifier environment invocation is not authoritative")
            if operation is not None:
                if expected_kind == "dependency_setup":
                    expected_hash = operation["command_sha256"]
                elif expected_kind == "fixture_destination_probe":
                    expected_hash = operation["destination_probe_command_sha256"]
                elif expected_kind == "fixture_stat":
                    expected_hash = operation["stat_command_sha256"]
                elif expected_kind == "fixture_digest":
                    expected_hash = operation["digest_command_sha256"]
                elif expected_kind == "source_resolve":
                    expected_hash = operation["resolve_command_sha256"]
                elif expected_kind.startswith("source_digest"):
                    expected_hash = operation["digest_command_sha256"]
                else:
                    expected_hash = operation["stat_command_sha256"]
                if invocation["command_sha256"] != expected_hash:
                    raise ValueError("verifier environment invocation identity changed")
            elif (
                expected_kind == "fixture_workdir_probe"
                and invocation["command_sha256"]
                != hashlib.sha256(b"pwd -P").hexdigest()
            ):
                raise ValueError("verifier fixture workdir probe identity changed")
        executions = dependency["executions"]
        if not isinstance(executions, list) or len(executions) != len(normalized_steps):
            raise ValueError("verifier dependency setup execution cardinality changed")
        for index, (execution, step) in enumerate(
            zip(executions, normalized_steps, strict=True)
        ):
            result = _require_exact_keys(
                execution,
                {"index", "kind", "command_sha256", "exit_code"},
                "verifier dependency setup execution",
            )
            if (
                type(result["index"]) is not int
                or type(result["exit_code"]) is not int
                or result["index"] != index
                or result["kind"] != step["kind"]
                or result["command_sha256"] != step["command_sha256"]
                or result["exit_code"] != 0
            ):
                raise ValueError(
                    "verifier dependency setup execution is not authoritative"
                )
        return True, json.dumps(value, sort_keys=True)
    except (TypeError, ValueError) as error:
        return False, f"invalid verifier readiness record: {error}"


def validate_verifier_readiness_ledger(
    ledger: Path, config: Path, expected_projection: dict[str, str]
) -> tuple[bool, str]:
    try:
        payload = _require_exact_keys(
            json.loads(ledger.read_text(encoding="utf-8")),
            {"schema", "config_sha256", "projection_sha256", "records"},
            "verifier readiness ledger",
        )
        if payload["schema"] != "astra.harness.verifier_readiness_ledger.v1":
            raise ValueError("verifier readiness ledger schema is not canonical")
        if payload["config_sha256"] != sha256(config):
            raise ValueError("verifier readiness ledger config identity changed")
        if payload["projection_sha256"] != canonical_json_sha256(expected_projection):
            raise ValueError("verifier readiness ledger projection identity changed")
        config_payload = json.loads(config.read_text(encoding="utf-8"))
        task_paths = [
            lexical_absolute(Path(item["path"])) for item in config_payload["tasks"]
        ]
        task_records = [
            {
                "path": str(path),
                "sha256": benchmark_task_tree_sha256(path),
                "test_sha256": official_verifier_sha256(path),
                "verifier_timeout_seconds": tomllib.loads(
                    (path / "task.toml").read_text(encoding="utf-8")
                )["verifier"]["timeout_sec"],
            }
            for path in task_paths
        ]
        records = payload["records"]
        if not isinstance(records, list) or len(records) != len(task_records):
            raise ValueError("verifier readiness ledger task cardinality changed")
        details = []
        for record, expected in zip(records, task_records, strict=True):
            ok, detail = validate_verifier_readiness_record(
                record,
                expected_task_sha256=expected["sha256"],
                expected_test_sha256=expected["test_sha256"],
                expected_projection=expected_projection,
                expected_dependency_setup_seconds=expected["verifier_timeout_seconds"],
            )
            if not ok:
                raise ValueError(detail)
            details.append(json.loads(detail))
        return True, json.dumps(
            {
                "ledger_sha256": sha256(ledger),
                "records": details,
            },
            sort_keys=True,
        )
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return False, f"invalid verifier readiness ledger: {error}"


def benchmark_provenance(
    config: Path,
    agent_digest: str | None,
    expected_source_revision: str | None = None,
) -> tuple[bool, str]:
    """Validate the immutable source/artifact identity carried by Harbor config."""
    try:
        payload = json.loads(config.read_text(encoding="utf-8"))
        agents = payload.get("agents") or []
        env = (agents[0] or {}).get("env") if isinstance(agents[0], dict) else None
        env = env if isinstance(env, dict) else {}
        source = str(env.get("ASTRA_EXPECTED_BUILD_GIT_SHA", "")).strip()
        artifact = str(env.get("ASTRA_HARNESS_BINARY_SHA256", "")).strip()
    except (OSError, ValueError, IndexError, TypeError) as error:
        return False, f"invalid config provenance: {error}"
    if not re.fullmatch(r"[0-9a-fA-F]{40}", source):
        return False, "ASTRA_EXPECTED_BUILD_GIT_SHA missing or not 40 hex characters"
    if (
        expected_source_revision is not None
        and source.lower() != expected_source_revision.lower()
    ):
        return False, "configured source revision does not match the checked-out HEAD"
    if not re.fullmatch(r"[0-9a-fA-F]{64}", artifact):
        return False, "ASTRA_HARNESS_BINARY_SHA256 missing or not 64 hex characters"
    if agent_digest is not None and artifact.lower() != agent_digest.lower():
        return False, "configured binary SHA does not match the selected agent artifact"
    return True, json.dumps(
        {"source_revision": source.lower(), "binary_sha256": artifact.lower()}
    )


def configured_custom_agent(config: Path) -> tuple[str, str] | None:
    """Return a Harbor module/class pair when the config uses a custom agent."""
    try:
        payload = json.loads(config.read_text(encoding="utf-8"))
        agents = payload.get("agents") or []
        name = str((agents[0] or {}).get("name", "")).strip()
    except (OSError, ValueError, IndexError, TypeError, AttributeError):
        return None
    if ":" not in name:
        return None
    module, attribute = (part.strip() for part in name.split(":", 1))
    if not module or not attribute:
        return None
    return module, attribute


def harbor_python(explicit: Path | None) -> Path | None:
    """Resolve Harbor's actual interpreter without importing Harbor itself."""
    if explicit is not None:
        # Keep a virtual environment's entrypoint path intact. Resolving its
        # Python symlink to the base interpreter can change sys.prefix and make
        # Harbor's own site-packages disappear.
        candidate = Path(os.path.abspath(explicit.expanduser()))
        return (
            candidate if candidate.is_file() and os.access(candidate, os.X_OK) else None
        )
    harbor = shutil.which("harbor")
    if not harbor:
        return None
    try:
        first_line = (
            Path(harbor).read_text(encoding="utf-8", errors="replace").splitlines()[0]
        )
    except (OSError, IndexError):
        return None
    if not first_line.startswith("#!"):
        return None
    candidate = Path(first_line[2:].strip()).expanduser()
    return candidate if candidate.is_file() and os.access(candidate, os.X_OK) else None


def probe_custom_agent_import(
    interpreter: Path,
    module: str,
    attribute: str,
    adapter_pythonpath: Path | None = None,
) -> tuple[bool, str]:
    """Import the configured adapter with Harbor's inherited environment.

    A sealed launcher addresses its snapshot through an inherited descriptor.
    That ``/proc/<launcher-pid>/fd`` spelling is valid to the launcher but not
    necessarily to the separate Harbor interpreter spawned by this probe.  An
    explicitly materialized, read-only snapshot path keeps this check about
    the exact sealed adapter instead of accidentally importing the worktree.
    """
    probe = (
        "import importlib, sys; "
        "obj = importlib.import_module(sys.argv[1]); "
        "getattr(obj, sys.argv[2]); "
        "print(sys.executable)"
    )
    extra_env: dict[str, str] = {}
    if adapter_pythonpath is not None:
        extra_env["PYTHONPATH"] = str(adapter_pythonpath)
    rc, out, err = run_text_with_env(
        [str(interpreter), "-c", probe, module, attribute], extra_env, timeout=10.0
    )
    detail = {
        "agent": f"{module}:{attribute}",
        "interpreter": str(interpreter),
        "resolved_interpreter": out if rc == 0 else None,
        "error": None if rc == 0 else (err or "import failed"),
    }
    return rc == 0, json.dumps(detail, ensure_ascii=False)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--agent", type=Path)
    parser.add_argument("--server", type=Path)
    parser.add_argument("--config", type=Path)
    parser.add_argument(
        "--harbor-python",
        type=Path,
        help="Harbor's Python interpreter; otherwise resolve it from the harbor launcher shebang",
    )
    parser.add_argument("--target", help="build target, e.g. x86_64-unknown-linux-musl")
    parser.add_argument(
        "--portable-probe-image",
        help="disposable container image in which to run the agent build-info probe",
    )
    parser.add_argument(
        "--health-url", help="already-started candidate server /health URL"
    )
    parser.add_argument("--models-url", help="authenticated server model-catalog URL")
    parser.add_argument(
        "--expected-build-git-sha",
        help="expected server health build_git_sha; defaults to repo HEAD",
    )
    parser.add_argument(
        "--provider-url",
        help="optional provider endpoint for a proxy reachability probe",
    )
    parser.add_argument(
        "--require-verifier-network",
        action="store_true",
        help="require and probe the launcher-owned verifier proxy projection",
    )
    parser.add_argument(
        "--expect-verifier-network-projection",
        action="store_true",
        help="require config.verifier.env to exactly match the validated projection",
    )
    parser.add_argument("--snapshot-root-fd", type=int)
    parser.add_argument("--snapshot-ledger-fd", type=int)
    parser.add_argument(
        "--probe-verifier-readiness",
        action="store_true",
        help="run each exact Harbor verifier in a disposable non-score environment",
    )
    parser.add_argument("--verifier-readiness-ledger", type=Path)
    parser.add_argument("--domain-state", type=Path)
    parser.add_argument("--json", action="store_true", help="emit JSON (default)")
    args = parser.parse_args()
    repo = args.repo.expanduser().resolve()
    results: list[dict[str, Any]] = []
    source_revision: str | None = None
    expected_verifier_network: dict[str, str] | None = None
    sealed_snapshot_root: Path | None = None

    if (args.snapshot_root_fd is None) != (args.snapshot_ledger_fd is None):
        results.append(
            check(
                "sealed_snapshot",
                False,
                "snapshot root and ledger descriptors must be supplied together",
            )
        )
    elif args.snapshot_root_fd is not None and args.snapshot_ledger_fd is not None:
        try:
            import sealed_run_snapshot

            snapshot = sealed_run_snapshot.open_snapshot(
                os.dup(args.snapshot_root_fd), os.dup(args.snapshot_ledger_fd)
            )
            try:
                snapshot.verify_open_ledger()
                # Docker cannot bind-mount an inherited /proc/*/fd file path.
                # Resolve the already-verified descriptor once to its immutable
                # directory, then require every materialized input to be the
                # same inode recorded by the sealed snapshot.
                root_link = os.readlink(f"/proc/self/fd/{snapshot.root_fd}")
                candidate_root = Path(root_link)
                if not candidate_root.is_dir():
                    raise RuntimeError(
                        "sealed snapshot descriptor has no materialized directory"
                    )
                sealed_snapshot_root = candidate_root
                snapshot_detail = json.dumps(
                    {
                        "schema": snapshot.ledger["schema"],
                        "source_revision": snapshot.ledger["source_revision"],
                        "ledger_sha256": snapshot._ledger_sha256,
                    },
                    sort_keys=True,
                )
            finally:
                snapshot.close()
            results.append(check("sealed_snapshot", True, snapshot_detail))
        except (ImportError, OSError, RuntimeError, ValueError, TypeError) as error:
            results.append(check("sealed_snapshot", False, str(error)))
    elif args.probe_verifier_readiness:
        results.append(
            check(
                "sealed_snapshot",
                False,
                "verifier readiness probe requires an open sealed snapshot",
            )
        )

    if args.require_verifier_network or args.expect_verifier_network_projection:
        try:
            expected_verifier_network = verifier_network_projection()
            results.append(
                check(
                    "verifier_network_projection_available",
                    True,
                    json.dumps(
                        {
                            "keys": sorted(expected_verifier_network),
                            "no_proxy": VERIFIER_NO_PROXY,
                        },
                        sort_keys=True,
                    ),
                )
            )
        except ValueError as error:
            results.append(
                check(
                    "verifier_network_projection_available",
                    False,
                    str(error),
                )
            )
    if not repo.is_dir():
        results.append(check("repo", False, f"not a directory: {repo}"))
    else:
        rc, out, err = run_text(
            ["git", "-C", str(repo), "status", "--porcelain", "--untracked-files=no"]
        )
        results.append(
            check("tracked_worktree_clean", rc == 0 and not out, err or out or "clean")
        )
        rc, out, err = run_text(["git", "-C", str(repo), "rev-parse", "HEAD"])
        if rc == 0 and len(out) == 40:
            source_revision = out.lower()
        results.append(
            check("source_revision", source_revision is not None, out or err)
        )

    for label, path in (("agent", args.agent), ("server", args.server)):
        if path is None:
            continue
        path = lexical_absolute(path)
        exists = path.is_file() and os.access(path, os.X_OK)
        digest = sha256(path) if exists else None
        build_info_ok = False
        build_info_detail = "source revision unavailable"
        if exists and source_revision is not None:
            build_info_ok, build_info_detail = probe_build_info(
                path,
                expected_git_sha=source_revision,
                expected_target=args.target if label == "agent" else None,
                expected_profile="debug",
            )
        results.append(
            check(
                f"{label}_binary",
                exists and build_info_ok,
                json.dumps(
                    {
                        "path": str(path),
                        "sha256": digest,
                        "build_info": build_info_detail,
                    },
                    ensure_ascii=False,
                ),
            )
        )

    if args.target:
        target = args.target.lower()
        if "musl" in target:
            native_tool = shutil.which("musl-gcc") or shutil.which(
                "x86_64-linux-musl-gcc"
            )
            cross_tool = shutil.which("cross")
            docker_tool = shutil.which("docker")
            tool_detail = {
                "musl_compiler": native_tool,
                "cross": cross_tool,
                "docker": docker_tool,
            }
            results.append(
                check(
                    "portable_build_toolchain",
                    bool(native_tool or (cross_tool and docker_tool)),
                    json.dumps(tool_detail, ensure_ascii=False),
                )
            )
        else:
            results.append(
                check("portable_build_toolchain", True, f"native target: {args.target}")
            )

    if args.agent is not None and args.target and "musl" in args.target.lower():
        image = args.portable_probe_image
        if not image:
            results.append(
                check(
                    "portable_container_probe_declared",
                    False,
                    "musl target requires --portable-probe-image before Harbor",
                )
            )
        elif not shutil.which("docker"):
            results.append(check("portable_container_probe", False, "docker not found"))
        else:
            image_rc, image_out, image_err = run_text(
                ["docker", "image", "inspect", image], timeout=10.0
            )
            if image_rc != 0:
                results.append(
                    check(
                        "portable_container_probe",
                        False,
                        f"probe image unavailable: {image_err or image_out or image}",
                    )
                )
            else:
                agent_path = lexical_absolute(args.agent)
                mount_agent_path = agent_path
                if sealed_snapshot_root is not None:
                    sealed_agent_path = sealed_snapshot_root / "agent" / "astra"
                    if not sealed_agent_path.is_file() or not os.path.samefile(
                        agent_path, sealed_agent_path
                    ):
                        results.append(
                            check(
                                "portable_container_probe",
                                False,
                                "sealed snapshot agent does not match the verified agent inode",
                            )
                        )
                        mount_agent_path = None
                    else:
                        mount_agent_path = sealed_agent_path
                if mount_agent_path is not None:
                    probe_rc, probe_out, probe_err = run_text(
                        [
                            "docker",
                            "run",
                            "--rm",
                            "--network=none",
                            "-v",
                            f"{mount_agent_path}:/astra-probe:ro",
                            image,
                            "/astra-probe",
                            "--build-info-json",
                        ],
                        timeout=30.0,
                    )
                    probe_ok = False
                    probe_detail = probe_err or "container probe failed"
                    if probe_rc == 0 and source_revision is not None:
                        probe_ok, probe_detail = validate_build_info(
                            probe_out,
                            expected_git_sha=source_revision,
                            expected_target=args.target,
                            expected_profile="debug",
                        )
                    results.append(
                        check(
                            "portable_container_probe",
                            probe_ok,
                            json.dumps(
                                {
                                    "image": image,
                                    "build_info": probe_detail,
                                },
                                ensure_ascii=False,
                            ),
                        )
                    )

    if args.config is not None:
        config = lexical_absolute(args.config)
        results.append(check("benchmark_config", config.is_file(), str(config)))
        agent_path = lexical_absolute(args.agent) if args.agent is not None else None
        agent_digest = (
            sha256(agent_path)
            if agent_path is not None and agent_path.is_file()
            else None
        )
        if config.is_file():
            if args.expect_verifier_network_projection:
                if expected_verifier_network is None:
                    shape_ok, shape_detail = (
                        False,
                        "verifier network projection is unavailable",
                    )
                else:
                    shape_ok, shape_detail = validate_benchmark_finalized_config(
                        config,
                        expected_verifier_network,
                        repo / "target" / "harbor-jobs",
                    )
            else:
                shape_ok, shape_detail = validate_benchmark_source_config(
                    config, repo / "target" / "harbor-jobs"
                )
            results.append(check("benchmark_source_config", shape_ok, shape_detail))
            ok, detail = benchmark_provenance(config, agent_digest, source_revision)
            results.append(check("benchmark_provenance", ok, detail))
            tasks_ok, tasks_detail = validate_benchmark_tasks(
                config,
                expected_verifier_network
                if args.expect_verifier_network_projection
                else None,
            )
            results.append(check("benchmark_tasks", tasks_ok, tasks_detail))
            models_ok, _, models_detail = configured_model_requirements(config)
            results.append(check("benchmark_models", models_ok, models_detail))
            config_digest = sha256(config)
            results.append(
                check(
                    "benchmark_config_sha256",
                    config_digest is not None,
                    config_digest or "unreadable",
                )
            )
            custom_agent = configured_custom_agent(config)
            if custom_agent is not None:
                interpreter = harbor_python(args.harbor_python)
                if interpreter is None:
                    results.append(
                        check(
                            "custom_agent_import",
                            False,
                            "could not resolve Harbor's Python interpreter; pass --harbor-python",
                        )
                    )
                else:
                    adapter_pythonpath = None
                    adapter_snapshot_ok = True
                    if sealed_snapshot_root is not None:
                        candidate = (
                            sealed_snapshot_root
                            / "control"
                            / "repo"
                            / "crates"
                            / "astra-test-harness"
                        )
                        if not candidate.is_dir():
                            results.append(
                                check(
                                    "custom_agent_import",
                                    False,
                                    "sealed snapshot does not contain the Harbor adapter",
                                )
                            )
                            adapter_snapshot_ok = False
                        else:
                            adapter_pythonpath = candidate
                    if adapter_snapshot_ok:
                        import_ok, import_detail = probe_custom_agent_import(
                            interpreter,
                            custom_agent[0],
                            custom_agent[1],
                            adapter_pythonpath,
                        )
                        results.append(
                            check("custom_agent_import", import_ok, import_detail)
                        )

    if args.probe_verifier_readiness:
        if (
            args.config is None
            or args.verifier_readiness_ledger is None
            or args.domain_state is None
        ):
            results.append(
                check(
                    "verifier_environment_readiness",
                    False,
                    "--probe-verifier-readiness requires --config, --verifier-readiness-ledger, and --domain-state",
                )
            )
        elif expected_verifier_network is None:
            results.append(
                check(
                    "verifier_environment_readiness",
                    False,
                    "exact verifier network projection is unavailable",
                )
            )
        elif not all(item["ok"] for item in results if item["required"]):
            results.append(
                check(
                    "verifier_environment_readiness",
                    False,
                    "earlier required preflight checks failed; no verifier container was started",
                )
            )
        else:
            interpreter = harbor_python(args.harbor_python)
            config = lexical_absolute(args.config)
            ledger = args.verifier_readiness_ledger.expanduser().resolve()
            helper = (
                sealed_snapshot_root
                / "control"
                / "repo"
                / "scripts"
                / "harness"
                / "verifier_readiness.py"
                if sealed_snapshot_root is not None
                else Path(__file__).with_name("verifier_readiness.py")
            )
            if interpreter is None:
                readiness_ok = False
                readiness_detail = "cannot resolve Harbor's exact Python interpreter"
            elif ledger.exists():
                readiness_ok = False
                readiness_detail = f"verifier readiness ledger already exists: {ledger}"
            else:
                rc, _, err = run_text(
                    [
                        str(interpreter),
                        str(helper),
                        "--config",
                        str(config),
                        "--ledger",
                        str(ledger),
                        "--domain-state",
                        str(args.domain_state.expanduser().resolve(strict=True)),
                        "--max-concurrency",
                        str(VERIFIER_READINESS_TASK_CONCURRENCY),
                    ],
                    timeout=verifier_readiness_timeout(
                        config,
                        max_concurrency=VERIFIER_READINESS_TASK_CONCURRENCY,
                    ),
                )
                if rc != 0:
                    readiness_ok = False
                    readiness_detail = err or "exact verifier readiness probe failed"
                else:
                    readiness_ok, readiness_detail = validate_verifier_readiness_ledger(
                        ledger, config, expected_verifier_network
                    )
            results.append(
                check("verifier_environment_readiness", readiness_ok, readiness_detail)
            )

    if args.health_url:
        expected = args.expected_build_git_sha
        if expected is None and repo.is_dir():
            rc, out, _ = run_text(["git", "-C", str(repo), "rev-parse", "HEAD"])
            expected = out if rc == 0 else None
        health_rc, health_out, health_err = run_text(
            [
                "curl",
                "--noproxy",
                "*",
                "--connect-timeout",
                "2",
                "--max-time",
                "5",
                "-fsS",
                args.health_url,
            ],
            timeout=8.0,
        )
        health_ok = False
        detail: str
        try:
            health = json.loads(health_out)
            health_ok = (
                health_rc == 0
                and health.get("status") in {"healthy", "degraded"}
                and health.get("database") == "connected"
                and (expected is None or health.get("build_git_sha") == expected)
            )
            detail = json.dumps(
                {
                    "status": health.get("status"),
                    "database": health.get("database"),
                    "build_git_sha": health.get("build_git_sha"),
                    "expected_build_git_sha": expected,
                },
                ensure_ascii=False,
            )
        except (ValueError, TypeError):
            detail = health_err or health_out or "invalid health response"
        results.append(check("server_health", health_ok, detail))

    if args.models_url:
        auth_token = os.environ.get("ASTRA_ACCESS_TOKEN") or os.environ.get(
            "ASTRA_AUTH_TOKEN"
        )
        if not auth_token:
            results.append(
                check(
                    "selected_model_route_ready",
                    False,
                    "server-side probe requires ASTRA_ACCESS_TOKEN or ASTRA_AUTH_TOKEN",
                )
            )
        elif args.config is None:
            results.append(
                check(
                    "selected_model_route_ready",
                    False,
                    "exact model probe requires --config",
                )
            )
        else:
            requirements_ok, requirements, requirements_detail = (
                configured_model_requirements(lexical_absolute(args.config))
            )
            if not requirements_ok:
                results.append(
                    check("selected_model_route_ready", False, requirements_detail)
                )
            else:
                model_ok, model_detail = fetch_model_catalog(
                    args.models_url, auth_token, requirements
                )
                results.append(
                    check("selected_model_route_ready", model_ok, model_detail)
                )
    else:
        # Provider keys are commonly persisted/encrypted in the server DB. The
        # post-start model-catalog probe is authoritative for that deployment;
        # do not fail the pre-start artifact gate merely because the shell does
        # not contain a provider key.
        results.append(
            check(
                "selected_model_route_ready",
                True,
                "deferred to authenticated exact-model catalog probe",
                required=False,
            )
        )
    results.append(
        check(
            "curl",
            shutil.which("curl") is not None,
            shutil.which("curl") or "not found",
        )
    )
    results.append(
        check(
            "python3",
            shutil.which("python3") is not None,
            shutil.which("python3") or "not found",
        )
    )
    results.append(
        check(
            "cgroup_v2",
            Path("/sys/fs/cgroup/cgroup.controllers").is_file(),
            "v2 mounted"
            if Path("/sys/fs/cgroup/cgroup.controllers").is_file()
            else "not available",
            required=False,
        )
    )
    proxy_present = any(
        os.environ.get(key)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy")
    )
    bypass = os.environ.get("NO_PROXY") or os.environ.get("no_proxy") or ""
    bypass_tokens = {item.strip().lower() for item in bypass.split(",") if item.strip()}
    local_bypass_ok = (
        not proxy_present
        or "*" in bypass_tokens
        or {
            "localhost",
            "127.0.0.1",
        }.issubset(bypass_tokens)
    )
    results.append(
        check(
            "proxy_local_bypass",
            local_bypass_ok,
            "configured"
            if local_bypass_ok
            else "proxy is set but NO_PROXY/no_proxy does not include localhost and 127.0.0.1",
            required=proxy_present,
        )
    )
    if args.provider_url:
        proxy_present = bool(
            os.environ.get("HTTPS_PROXY")
            or os.environ.get("https_proxy")
            or os.environ.get("HTTP_PROXY")
            or os.environ.get("http_proxy")
        )
        provider_proxy_env = {
            key: value
            for key in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy")
            if (value := os.environ.get(key)) is not None
        }
        provider_rc, provider_out, provider_err = run_text_with_env(
            [
                "curl",
                "--connect-timeout",
                "3",
                "--max-time",
                "8",
                "-fsSI",
                args.provider_url,
            ],
            provider_proxy_env,
            timeout=12.0,
        )
        results.append(
            check(
                "provider_reachability",
                provider_rc == 0,
                json.dumps(
                    {
                        "proxy_configured": proxy_present,
                        "url": args.provider_url,
                        "probe_exit": provider_rc,
                        "error": provider_err or None,
                    },
                    ensure_ascii=False,
                ),
            )
        )

    payload = {
        "ok": all(item["ok"] for item in results if item["required"]),
        "checks": results,
    }
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return 0 if payload["ok"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
