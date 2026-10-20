"""Exercise the actual checkout-free PR assessment workflow step."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from test_release_build_shells import workflow_run_script


class CapabilityAssessmentTests(unittest.TestCase):
    def test_assessment_contract_on_pr_events(self):
        script = workflow_run_script(
            ".github/workflows/pr-title.yml", "Validate capability assessment"
        )
        valid = "Unchanged: MCP configuration remains Beta for CLI and Offline."
        cases = [
            (None, False),
            ("## Summary\nA new public route.", False),
            ("## Capability assessment\n<!-- Unchanged: template guidance. -->", False),
            ("## Capability assessment\nN/A", False),
            ("## Capability assessment\nN/A:", False),
            ("## Capability assessment\nUnchanged: TODO explain impact", False),
            ("## Capability assessment\nUpdated: <explanation>", False),
            ("## Capability assessment\nUpdated: ___", False),
            ("## Capability assessment\nUnknown: no impact", False),
            (f"## Capability assessment\n{valid}\n## Capability assessment\n{valid}", False),
            (f"<!--\n## Capability assessment\n{valid}\n-->", False),
            (f"<!--\n## Capability assessment\n{valid}", False),
            (f"```html\n<!--\n```\n## Capability assessment\n{valid}", True),
            (f"<!--\n```\n## Capability assessment\nFake\n-->\n## Capability assessment\n{valid}", True),
            (f"```markdown\n## Capability assessment\n{valid}\n```", False),
            (f"## Capability assessment\n## Verification\n{valid}", False),
            (f"## Capability assessment\n{valid}\n## Verification\nTests passed.", True),
            ("## Capability assessment\nUpdated: Agent chat remains Beta for CLI, Server and Web; the reference now records the new limit.", True),
            ("## Capability assessment\nN/A: Version-only release preparation changes no capability behavior.", True),
            (f"~~~markdown\n## Capability assessment\nExample\n~~~~\n## Capability assessment\n{valid}", True),
        ]
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            for index, (body, accepted) in enumerate(cases):
                with self.subTest(case=index):
                    event_path.write_text(json.dumps({
                        "action": "edited",
                        "pull_request": {"body": body},
                    }))
                    result = subprocess.run(
                        ["bash", "-eu", "-c", script],
                        env={**os.environ, "GITHUB_EVENT_PATH": str(event_path)},
                        capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(result.returncode == 0, accepted, result.stderr)

    def test_bot_and_release_bodies_are_data_and_require_the_same_assessment(self):
        script = workflow_run_script(
            ".github/workflows/pr-title.yml", "Validate capability assessment"
        )
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            sentinel = Path(directory) / "must-not-exist"
            for author, title in [
                ("dependabot[bot]", "build(deps): update dependency"),
                ("maintainer", "chore(release): prepare version"),
            ]:
                for assessed in (False, True):
                    body = f"## Summary\n{title}\n"
                    if assessed:
                        body += (
                            "## Capability assessment\n"
                            f"Unchanged: Existing capability profiles apply; $(touch {sentinel}) `touch {sentinel}` are inert PR text."
                        )
                    event_path.write_text(json.dumps({"pull_request": {
                        "user": {"login": author}, "title": title, "body": body,
                    }}))
                    result = subprocess.run(
                        ["bash", "-eu", "-c", script],
                        env={**os.environ, "GITHUB_EVENT_PATH": str(event_path)},
                        capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(result.returncode == 0, assessed, result.stderr)
                    self.assertFalse(sentinel.exists())
                    self.assertNotIn(str(sentinel), result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
