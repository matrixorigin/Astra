#!/usr/bin/env python3
"""Closed, secret-free Docker Compose environment needed only for recovery."""

from __future__ import annotations

from collections.abc import Mapping


RECOVERY_COMPOSE_ENV_KEYS = frozenset(
    {
        "MAIN_IMAGE_NAME",
        "CONTEXT_DIR",
        "PREBUILT_IMAGE_NAME",
        "EGRESS_CONTROL_SIDECAR_IMAGE_NAME",
        "EGRESS_CONTROL_INITIAL_NETWORK_MODE",
        "EGRESS_CONTROL_INITIAL_ALLOWED_HOSTS",
        "CPUS",
        "MEMORY",
        "HARBOR_CONTAINER_NAME",
        "ENV_VERIFIER_LOGS_PATH",
        "HOST_VERIFIER_LOGS_PATH",
        "ENV_AGENT_LOGS_PATH",
        "HOST_AGENT_LOGS_PATH",
        "ENV_ARTIFACTS_PATH",
        "HOST_ARTIFACTS_PATH",
    }
)


def project_recovery_compose_env(value: object) -> dict[str, str]:
    if not isinstance(value, Mapping):
        raise ValueError("Docker recovery environment is not a mapping")
    if not all(
        isinstance(key, str) and isinstance(item, str) for key, item in value.items()
    ):
        raise ValueError("Docker recovery environment is not textual")
    unexpected = set(value) - RECOVERY_COMPOSE_ENV_KEYS
    if unexpected:
        raise ValueError(
            "Docker recovery environment contains non-recovery keys: "
            + ", ".join(sorted(unexpected))
        )
    if any("\0" in item or "\n" in item or "\r" in item for item in value.values()):
        raise ValueError("Docker recovery environment contains control characters")
    return {key: value[key] for key in sorted(value)}
