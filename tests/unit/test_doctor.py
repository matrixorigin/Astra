"""Tests for /doctor diagnostic command."""

from io import StringIO
from unittest.mock import MagicMock

from rich.console import Console

from cli.ui.doctor import run_doctor


def _console() -> tuple[Console, StringIO]:
    buf = StringIO()
    return Console(file=buf, force_terminal=True, width=80), buf


class TestDoctor:
    def test_python_version_check(self):
        console, buf = _console()
        checks = run_doctor(console, client=None)
        python_check = next(c for c in checks if "Python" in c[0])
        assert python_check[1] is True  # we're running 3.11+

    def test_rich_importable(self):
        console, buf = _console()
        checks = run_doctor(console, client=None)
        rich_check = next(c for c in checks if c[0] == "rich")
        assert rich_check[1] is True

    def test_prompt_toolkit_importable(self):
        console, buf = _console()
        checks = run_doctor(console, client=None)
        pt_check = next(c for c in checks if c[0] == "prompt_toolkit")
        assert pt_check[1] is True

    def test_no_client_api_fails(self):
        console, buf = _console()
        checks = run_doctor(console, client=None)
        api_check = next(c for c in checks if "API" in c[0])
        assert api_check[1] is False

    def test_no_client_auth_fails(self):
        console, buf = _console()
        checks = run_doctor(console, client=None)
        auth_check = next(c for c in checks if "Auth" in c[0])
        assert auth_check[1] is False

    def test_with_authenticated_client(self):
        console, buf = _console()
        client = MagicMock()
        client.base_url = "http://localhost:8000"
        client.ensure_authenticated.return_value = True
        checks = run_doctor(console, client=client)
        auth_check = next(c for c in checks if "Auth" in c[0])
        assert auth_check[1] is True

    def test_output_contains_table(self):
        console, buf = _console()
        run_doctor(console, client=None)
        output = buf.getvalue()
        assert "mo-agent doctor" in output
        assert "Python" in output

    def test_with_unreachable_api(self):
        console, buf = _console()
        client = MagicMock()
        client.base_url = "http://localhost:99999"
        client.ensure_authenticated.side_effect = Exception("connection refused")
        checks = run_doctor(console, client=client)
        api_check = next(c for c in checks if "API" in c[0])
        assert api_check[1] is False
