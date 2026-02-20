"""Unit tests for DockerRuntime (mocked Docker client)."""

from unittest.mock import Mock, patch, MagicMock
import pytest
from core.runtime import ExecutionResult, ResourceProfile
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

    def test_health_check(self, runtime):
        runtime.client.ping.return_value = True
        assert runtime.health_check() is True

    def test_health_check_failure(self, runtime):
        runtime.client.ping.side_effect = Exception("no docker")
        assert runtime.health_check() is False

    def test_execute_success(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 0}
        container.logs.side_effect = [b"hello\n", b""]
        runtime.client.containers.run.return_value = container

        result = runtime.execute("print('hello')")
        assert result.exit_code == 0
        assert result.stdout == "hello\n"
        container.remove.assert_called_once_with(force=True)

    def test_execute_failure(self, runtime):
        container = Mock()
        container.wait.return_value = {"StatusCode": 1}
        container.logs.side_effect = [b"", b"error\n"]
        runtime.client.containers.run.return_value = container

        result = runtime.execute("bad code")
        assert result.exit_code == 1
        assert result.stderr == "error\n"

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
