"""TrustMem Cloud v1 test configuration."""

import os
import pytest


def pytest_configure(config):
    """Fail fast if local embedding is configured in CI."""
    ci = os.environ.get("CI") or os.environ.get("GITHUB_ACTIONS")
    provider = os.environ.get("TRUSTMEM_EMBEDDING_PROVIDER", "local")
    if ci and provider == "local":
        pytest.exit(
            "CI environment detected but TRUSTMEM_EMBEDDING_PROVIDER=local. "
            "Set TRUSTMEM_EMBEDDING_PROVIDER=openai and TRUSTMEM_EMBEDDING_API_KEY secret.",
            returncode=1,
        )


def pytest_collection_modifyitems(items):
    """Force governance tests to run in the same xdist group."""
    for item in items:
        if "Governance" in item.nodeid or "AdminStatsAccuracy" in item.nodeid:
            item.add_marker(pytest.mark.xdist_group("governance"))
