"""Unit tests for FirecrackerRuntime (mocked — no KVM on macOS)."""

from unittest.mock import Mock, patch, MagicMock
import pytest
from core.runtime import ExecutionResult, ResourceProfile, IsolationLevel
from core.runtime.firecracker_runtime import FirecrackerRuntime, _is_available


class TestFirecrackerRuntime:
    @pytest.fixture
    def runtime(self):
        return FirecrackerRuntime(
            kernel_path="/fake/vmlinux",
            rootfs_path="/fake/rootfs.ext4",
            firecracker_bin="firecracker",
        )

    def test_capabilities(self, runtime):
        cap = runtime.capabilities
        assert cap.isolation == IsolationLevel.MICROVM
        assert cap.network_isolatable is True
        assert cap.filesystem_isolated is True
        assert cap.resource_limits is True
        assert cap.reproducible is True

    def test_supported_languages(self, runtime):
        assert runtime.supported_languages == ["python"]

    def test_unsupported_language(self, runtime):
        result = runtime.execute("code", language="ruby")
        assert result.exit_code == 1
        assert "Unsupported" in result.stderr

    def test_health_check_no_kvm(self, runtime):
        """On macOS (no /dev/kvm), health_check returns False."""
        assert runtime.health_check() is False

    def test_is_available_checks_platform(self):
        with patch("core.runtime.firecracker_runtime.platform") as mock_plat:
            mock_plat.system.return_value = "Darwin"
            assert _is_available() is False

    def test_is_available_checks_kvm(self):
        with patch("core.runtime.firecracker_runtime.platform") as mock_plat, \
             patch("core.runtime.firecracker_runtime.os.path.exists") as mock_exists, \
             patch("core.runtime.firecracker_runtime.shutil.which") as mock_which:
            mock_plat.system.return_value = "Linux"
            mock_exists.return_value = False  # no /dev/kvm
            assert _is_available() is False

    def test_is_available_checks_binary(self):
        with patch("core.runtime.firecracker_runtime.platform") as mock_plat, \
             patch("core.runtime.firecracker_runtime.os.path.exists") as mock_exists, \
             patch("core.runtime.firecracker_runtime.shutil.which") as mock_which:
            mock_plat.system.return_value = "Linux"
            mock_exists.return_value = True  # /dev/kvm exists
            mock_which.return_value = None   # no firecracker binary
            assert _is_available() is False

    def test_is_available_all_present(self):
        with patch("core.runtime.firecracker_runtime.platform") as mock_plat, \
             patch("core.runtime.firecracker_runtime.os.path.exists") as mock_exists, \
             patch("core.runtime.firecracker_runtime.shutil.which") as mock_which:
            mock_plat.system.return_value = "Linux"
            mock_exists.return_value = True
            mock_which.return_value = "/usr/bin/firecracker"
            assert _is_available() is True

    @patch("core.runtime.firecracker_runtime.subprocess.Popen")
    @patch.object(FirecrackerRuntime, "_wait_for_socket", return_value=False)
    def test_execute_socket_timeout(self, mock_wait, mock_popen, runtime):
        mock_proc = Mock()
        mock_proc.poll.return_value = None
        mock_proc.wait.return_value = None
        mock_popen.return_value = mock_proc

        result = runtime.execute("print('hi')")
        assert result.exit_code == 1
        assert "socket not ready" in result.stderr
        mock_proc.kill.assert_called()

    @patch("core.runtime.firecracker_runtime.subprocess.Popen")
    @patch.object(FirecrackerRuntime, "_wait_for_socket", return_value=True)
    @patch.object(FirecrackerRuntime, "_api_put")
    def test_execute_wall_timeout(self, mock_api, mock_wait, mock_popen, runtime):
        import subprocess as sp
        mock_proc = Mock()
        mock_proc.poll.return_value = None
        # First wait() → timeout; second wait() in finally → ok
        mock_proc.wait.side_effect = [sp.TimeoutExpired("fc", 5), None]
        mock_popen.return_value = mock_proc

        result = runtime.execute("while True: pass", resources=ResourceProfile(max_wall_seconds=5))
        assert result.exit_code == 137
        assert "timed out" in result.stderr


class TestCreateRuntimeWithFirecracker:
    def test_firecracker_selected_when_available(self):
        """create_runtime picks Firecracker when it passes health_check."""
        from core.runtime import create_runtime
        with patch("core.runtime.firecracker_runtime.FirecrackerRuntime") as MockFC:
            mock_rt = MockFC.return_value
            mock_rt.health_check.return_value = True
            mock_rt.capabilities = FirecrackerRuntime(
                kernel_path="x", rootfs_path="x"
            ).capabilities
            rt = create_runtime(min_isolation=IsolationLevel.PROCESS)
            assert rt is mock_rt

    def test_firecracker_skipped_when_unhealthy(self):
        """Falls through to Docker/Subprocess when Firecracker unavailable."""
        from core.runtime import create_runtime
        from core.runtime.subprocess_runtime import SubprocessRuntime
        with patch("core.runtime.firecracker_runtime.FirecrackerRuntime") as MockFC, \
             patch("core.runtime.docker_runtime.DockerRuntime") as MockDocker:
            MockFC.return_value.health_check.return_value = False
            MockDocker.return_value.health_check.return_value = False
            rt = create_runtime(min_isolation=IsolationLevel.PROCESS)
            assert isinstance(rt, SubprocessRuntime)

    def test_microvm_required_but_unavailable(self):
        """Raises when MICROVM required but Firecracker not available."""
        from core.runtime import create_runtime
        with patch("core.runtime.firecracker_runtime.FirecrackerRuntime") as MockFC:
            MockFC.return_value.health_check.return_value = False
            with pytest.raises(RuntimeError, match="No runtime available"):
                create_runtime(min_isolation=IsolationLevel.MICROVM)
