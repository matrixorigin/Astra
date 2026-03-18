"""Tests for skill runner error handling."""

import json
import subprocess
import sys

import pytest


class TestSkillRunnerErrors:
    """Test runner.py error messages include diagnostic hints."""

    def test_invalid_json_input(self):
        """Invalid JSON input returns error without hint."""
        result = subprocess.run(
            [sys.executable, "-m", "core.skills.runner", "--skill", "test", "--inputs", "not-json"],
            capture_output=True,
            text=True,
        )
        assert result.returncode == 1
        error = json.loads(result.stderr)
        assert "Invalid JSON" in error["error"]
        # No hint for JSON parse errors
        assert "hint" not in error

    def test_skill_not_found_includes_hint(self):
        """Skill not found error includes diagnostic hint."""
        result = subprocess.run(
            [
                sys.executable,
                "-m",
                "core.skills.runner",
                "--skill",
                "nonexistent_xyz",
                "--inputs",
                "{}",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 1
        error = json.loads(result.stderr)
        assert "not found" in error["error"]
        assert "hint" in error
        assert "diagnose_skills" in error["hint"]


class TestSkillRunnerIndex:
    def test_runner_skill_index_finds_known_skills(self):
        from core.skills.runner_index import runner_skill_exists

        assert runner_skill_exists("introspection")
        assert runner_skill_exists("feedback_classifier")
        assert not runner_skill_exists("nonexistent_xyz")


class TestSkillNotFoundError:
    """Test SkillNotFoundError has hint attribute."""

    def test_has_hint_attribute(self):
        from core.exceptions import SkillNotFoundError

        err = SkillNotFoundError("test_skill")
        assert hasattr(err, "hint")
        assert "diagnose_skills" in err.hint

    def test_error_message(self):
        from core.exceptions import SkillNotFoundError

        err = SkillNotFoundError("my_skill", version="1.0.0")
        assert "my_skill" in str(err)
        assert "1.0.0" in str(err)
