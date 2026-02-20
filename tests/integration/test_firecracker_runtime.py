"""Real Firecracker runtime integration tests.

Skipped automatically on machines without KVM / firecracker binary / kernel+rootfs.
On a properly provisioned Linux host, these run real microVMs.
"""

import pytest
from core.runtime.firecracker_runtime import FirecrackerRuntime, _is_available
from core.runtime import ResourceProfile, IsolationLevel, create_runtime

_skip = pytest.mark.skipif(
    not _is_available(),
    reason="Firecracker not available (needs Linux + /dev/kvm + firecracker binary)",
)


@pytest.fixture
def runtime():
    rt = FirecrackerRuntime()
    if not rt.health_check():
        pytest.skip("Firecracker health_check failed (missing kernel/rootfs)")
    return rt


@_skip
class TestFirecrackerReal:
    def test_health_check(self, runtime):
        assert runtime.health_check() is True

    def test_capabilities(self, runtime):
        cap = runtime.capabilities
        assert cap.isolation == IsolationLevel.MICROVM
        assert cap.network_isolatable is True
        assert cap.filesystem_isolated is True

    def test_basic_execution(self, runtime):
        result = runtime.execute('print("hello from microvm")')
        assert result.exit_code == 0
        assert "hello from microvm" in result.stdout

    def test_network_isolated(self, runtime):
        result = runtime.execute(
            'import urllib.request; urllib.request.urlopen("http://example.com")',
            resources=ResourceProfile(network_enabled=False),
        )
        assert result.exit_code != 0

    def test_env_vars(self, runtime):
        result = runtime.execute(
            'import os; print(os.environ.get("MY_VAR", "missing"))',
            env={"MY_VAR": "from_microvm"},
        )
        assert result.exit_code == 0
        assert "from_microvm" in result.stdout

    def test_wall_timeout(self, runtime):
        result = runtime.execute(
            "import time; time.sleep(999)",
            resources=ResourceProfile(max_wall_seconds=3),
        )
        assert result.exit_code == 137
        assert "timed out" in result.stderr

    def test_create_runtime_selects_firecracker(self):
        rt = create_runtime(min_isolation=IsolationLevel.MICROVM)
        assert isinstance(rt, FirecrackerRuntime)
