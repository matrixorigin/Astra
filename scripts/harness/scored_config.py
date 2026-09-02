#!/usr/bin/env python3
"""Generate a closed, explicitly selected three-case scored benchmark plan."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

import preflight


class ConfigError(RuntimeError):
    pass


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def canonical_payload(
    *,
    repo: Path,
    revision: str,
    agent: Path,
    model: str = preflight.DEFAULT_BENCHMARK_SELECTOR,
    tasks: list[Path] | None = None,
) -> dict:
    if tasks is None:
        raise ConfigError(
            "scored plan requires at least three explicitly selected tasks; "
            "automatic default reuse is prohibited"
        )
    tasks = [path.resolve(strict=True) for path in tasks]
    if len(tasks) < 3 or len(set(tasks)) != len(tasks):
        raise ConfigError("canonical scored plan requires at least three unique tasks")
    model = model.strip()
    base, thinking = preflight.resolve_model_selector(model)
    if not base or thinking not in {None, "high"}:
        raise ConfigError(
            "scored plan requires an explicit model selector with no thinking suffix or thinking:high"
        )
    task_set_sha256, _ = preflight.benchmark_task_set_sha256(tasks)
    payload = {
        "jobs_dir": str((repo / "target" / "harbor-jobs").resolve()),
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
        "retry": {"max_retries": 0},
        "environment": {"type": "docker", "force_build": False, "delete": True},
        "verifier": {"disable": False},
        "metrics": [],
        "agents": [
            {
                "name": preflight.EXPECTED_BENCHMARK_AGENT,
                "model_name": model,
                "env": {
                    "ASTRA_EXPECTED_BUILD_GIT_SHA": revision,
                    "ASTRA_HARNESS_BINARY_SHA256": _sha256(agent),
                    "ASTRA_HARNESS_BUILD_PROFILE": "debug",
                    "ASTRA_HARNESS_TASK_SET_SHA256": task_set_sha256,
                    "ASTRA_HARBOR_HTTP_PROXY": "${ASTRA_HARBOR_HTTP_PROXY}",
                    "ASTRA_HARBOR_HTTPS_PROXY": "${ASTRA_HARBOR_HTTPS_PROXY}",
                },
            }
        ],
        "datasets": [],
        "tasks": [{"path": str(path)} for path in tasks],
        "artifacts": [],
        "extra_instruction_paths": [],
        "source_jobs": [],
    }
    ok, detail = preflight.validate_benchmark_source_config(
        _write_for_validation(payload), repo / "target" / "harbor-jobs"
    )
    if not ok:
        raise ConfigError(detail)
    return payload


def _write_for_validation(payload: dict) -> Path:
    # Validation accepts a Path.  /proc/self/fd keeps the bytes in this process
    # and avoids blessing an intermediate pathname.
    raw = (json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if not hasattr(os, "memfd_create"):
        raise ConfigError(
            "Linux memfd_create is required for canonical config validation"
        )
    descriptor = os.memfd_create("astra-scored-config", os.MFD_CLOEXEC)
    os.write(descriptor, raw)
    os.lseek(descriptor, 0, os.SEEK_SET)
    path = Path(f"/proc/self/fd/{descriptor}")
    # Attach the descriptor to the Path for the duration of immediate use.
    _VALIDATION_FDS.append(descriptor)
    return path


_VALIDATION_FDS: list[int] = []


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--agent", type=Path, required=True)
    parser.add_argument("--model", default=preflight.DEFAULT_BENCHMARK_SELECTOR)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--task", action="append", type=Path)
    args = parser.parse_args()
    try:
        payload = canonical_payload(
            repo=args.repo.resolve(strict=True),
            revision=args.revision,
            agent=args.agent.resolve(strict=True),
            model=args.model,
            tasks=args.task,
        )
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(args.output, flags, 0o400)
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(payload, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        print(json.dumps({"ok": True, "output": str(args.output)}, sort_keys=True))
        return 0
    except (OSError, ValueError, ConfigError) as error:
        print(
            f"astra harness: canonical config generation failed: {error}",
            file=sys.stderr,
        )
        return 78
    finally:
        for descriptor in _VALIDATION_FDS:
            os.close(descriptor)


if __name__ == "__main__":
    raise SystemExit(main())
