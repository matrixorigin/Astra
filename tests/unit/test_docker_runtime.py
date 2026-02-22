"""Unit tests for DockerRuntime (mocked Docker client)."""

from unittest.mock import Mock, patch, MagicMock
import pytest
from core.runtime import ExecutionResult, ResourceProfile, IsolationLevel
from core.runtime.docker_runtime import DockerRuntime


@pytest.fixture
def runtime():
    with patch("core.runtime.docker_runtime.docker") as mock_docker:
        rt = DockerRuntime(image="python:3.11-slim")
        rt._client = mock_docker.from_env.return_value
        yield rt


class TestDockerRuntime:
    def test_supported_languages(self, runtime):
        assert runtime.supported_languages == ["python"]

    def test_unsupported_language(self, runtime):
        result = runtime.execute("code", language="ruby")
        assert result.exit_code == 1
        assert "Unsupported" in result.stderr

    def test_health_check_failure(self, runtime):
        runtime.client.ping.side_effect = Exception("no docker")
        assert runtime.health_check() is False

    def test_execute_timeout(self, runtime):
        container = Mock()
        container.wait.side_effect = Exception("timeout")
        runtime.client.containers.run.return_value = container

        result = runtime.execute("while True: pass", resources=ResourceProfile(max_wall_seconds=5))
        assert result.exit_code == 137
        assert "timed out" in result.stderr
        container.kill.assert_called_once()

    def test_execute_network_disabled(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        container.logs.side_effect = [b"", b""]
        runtime.client.containers.run.return_value = container

        runtime.execute("print(1)", resources=ResourceProfile(network_enabled=False))
        call_kwargs = runtime.client.containers.run.call_args[1]
        assert call_kwargs["network_mode"] == "none"

    def test_execute_network_enabled(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        container.logs.side_effect = [b"", b""]
        runtime.client.containers.run.return_value = container

        runtime.execute("print(1)", resources=ResourceProfile(network_enabled=True))
        call_kwargs = runtime.client.containers.run.call_args[1]
        assert call_kwargs["network_mode"] == "bridge"

    def test_execute_truncates_output(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        big_output = b"x" * 2_000_000
        container.logs.side_effect = [big_output, b""]
        runtime.client.containers.run.return_value = container

        result = runtime.execute("print('x'*2000000)", resources=ResourceProfile(max_output_bytes=100))
        assert result.truncated is True
        assert len(result.stdout) == 100

    def test_execute_passes_env(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        container.logs.side_effect = [b"", b""]
        runtime.client.containers.run.return_value = container

        runtime.execute("print(1)", env={"MO_DATABASE": "test"})
        call_kwargs = runtime.client.containers.run.call_args[1]
        assert call_kwargs["environment"] == {"MO_DATABASE": "test"}

    def test_execute_security_opts(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        container.logs.side_effect = [b"", b""]
        runtime.client.containers.run.return_value = container

        runtime.execute("print(1)")
        call_kwargs = runtime.client.containers.run.call_args[1]
        assert call_kwargs["cap_drop"] == ["ALL"]
        assert call_kwargs["security_opt"] == ["no-new-privileges"]
        assert call_kwargs["read_only"] is True
        assert call_kwargs["pids_limit"] == 64

    def test_auto_pull_image(self, runtime):
        from docker.errors import ImageNotFound
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        container.logs.side_effect = [b"ok\n", b""]

        # First call: ImageNotFound, second call: success
        runtime.client.containers.run.side_effect = [
            ImageNotFound("not found"),
            container,
        ]

        result = runtime.execute("print('ok')")
        runtime.client.images.pull.assert_called_once_with("python:3.11-slim")
        assert result.exit_code == 0


class TestSubprocessRuntimeCapabilities:
    def test_capabilities(self):
        from core.runtime.subprocess_runtime import SubprocessRuntime
        rt = SubprocessRuntime()
        cap = rt.capabilities
        assert cap.isolation == IsolationLevel.PROCESS
        assert cap.network_isolatable is False
        assert cap.filesystem_isolated is False


class TestCreateRuntime:
    """All tests mock both Firecracker and Docker to isolate selection logic."""

    def _patch_fc_unavailable(self):
        return patch("core.runtime.firecracker_runtime.FirecrackerRuntime",
                      **{"return_value.health_check.return_value": False})

    def test_fallback_to_subprocess(self):
        """When Docker and Firecracker unavailable, falls back to SubprocessRuntime."""
        from core.runtime import create_runtime
        from core.runtime.subprocess_runtime import SubprocessRuntime
        with self._patch_fc_unavailable(), \
             patch("core.runtime.docker_runtime.DockerRuntime") as MockDocker:
            MockDocker.return_value.health_check.return_value = False
            rt = create_runtime(min_isolation=IsolationLevel.PROCESS)
            assert isinstance(rt, SubprocessRuntime)

    def test_raises_when_no_runtime_satisfies(self):
        """Raises RuntimeError when no runtime meets requirements."""
        from core.runtime import create_runtime
        with self._patch_fc_unavailable(), \
             patch("core.runtime.docker_runtime.DockerRuntime") as MockDocker:
            MockDocker.return_value.health_check.return_value = False
            with pytest.raises(RuntimeError, match="No runtime available"):
                create_runtime(min_isolation=IsolationLevel.CONTAINER)

    def test_subprocess_rejected_for_network_isolation(self):
        """SubprocessRuntime can't isolate network, so it's rejected."""
        from core.runtime import create_runtime
        with self._patch_fc_unavailable(), \
             patch("core.runtime.docker_runtime.DockerRuntime") as MockDocker:
            MockDocker.return_value.health_check.return_value = False
            with pytest.raises(RuntimeError):
                create_runtime(require_network_isolation=True)
