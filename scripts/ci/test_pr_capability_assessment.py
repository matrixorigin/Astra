"""Exercise the actual checkout-free PR metadata workflow step."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from test_release_build_shells import workflow_run_script


class CapabilityAssessmentTests(unittest.TestCase):
    def setUp(self):
        self.script = workflow_run_script(
            ".github/workflows/pr-title.yml", "Validate capability assessment"
        )
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.event_path = Path(self.directory.name) / "event.json"

    def run_event(self, body, **metadata):
        self.event_path.write_text(json.dumps({
            "action": "edited",
            "pull_request": {**metadata, "body": body},
        }))
        return subprocess.run(
            ["bash", "-eu", "-c", self.script],
            env={**os.environ, "GITHUB_EVENT_PATH": str(self.event_path)},
            capture_output=True, text=True, timeout=5,
        )

    def test_first_line_contract(self):
        valid = "Capability assessment: Unchanged: MCP remains Beta for CLI and Offline."
        prefix = "Capability assessment: N/A: "
        cases = [
            (None, False),
            (42, False),
            ({"body": valid}, False),
            ("", False),
            ("## Capability assessment\nUnchanged: MCP remains Beta.", False),
            (f"\n{valid}", False),
            (f" {valid}", False),
            (f"\ufeff{valid}", False),
            (valid.replace("assessment: ", "assessment:"), False),
            (valid.replace("Unchanged: ", "Unchanged:"), False),
            (valid.replace("Unchanged", "Unknown"), False),
            (prefix, False),
            (prefix + "\nAn explanation on a later line.", False),
            (prefix + "TODO explain impact", False),
            (prefix + "TBD", False),
            (prefix + "<explanation>", False),
            (prefix + "None", False),
            (prefix + "___", False),
            (prefix + "x" * (1024 - len(prefix)), True),
            (prefix + "x" * (1025 - len(prefix)), False),
            (valid, True),
            (valid + "\r\n## Summary\nA change.", True),
            (valid + "\r", False),
            ("Capability assessment: Updated: Agent chat remains Beta; the reference records the new limit.", True),
            (prefix + "仅修改文档，无能力变化。", True),
            (f"{valid}\n{valid}", True),  # Only the first line is authoritative.
            (f"Summary\n{valid}", False),
            (f"    {valid}", False),
            (f"```\n{valid}\n```", False),
            (f"<!--\n{valid}\n-->", False),
        ]
        # E31: ordinary Markdown cannot change the meaning of the metadata.
        for markdown in [
            "Document the literal `<!--` marker.",
            r"Document the literal \<!-- marker.",
            "```html\n<!--\n```",
            "<!-- An unclosed comment",
            "## Capability assessment\n\n    Unchanged: An indented example.",
        ]:
            cases.append((f"{valid}\n\n## Summary\n{markdown}", True))
        for index, (body, accepted) in enumerate(cases):
            with self.subTest(case=index):
                result = self.run_event(body)
                self.assertEqual(result.returncode == 0, accepted, result.stderr)

    def test_template_requires_an_explicit_assessment(self):
        template = Path(".github/PULL_REQUEST_TEMPLATE.md").read_text()
        self.assertNotEqual(self.run_event(template).returncode, 0)
        completed = template.replace(
            template.splitlines()[0],
            "Capability assessment: N/A: Documentation-only edits change no product capability.",
            1,
        )
        self.assertEqual(self.run_event(completed).returncode, 0)

    def test_bot_and_release_bodies_are_inert_data_with_the_same_contract(self):
        sentinel = Path(self.directory.name) / "must-not-exist"
        for author, title in [
            ("dependabot[bot]", "build(deps): update dependency"),
            ("maintainer", "chore(release): prepare version"),
        ]:
            for assessed in (False, True):
                with self.subTest(author=author, assessed=assessed):
                    body = f"## Summary\n{title}\n"
                    if assessed:
                        body = (
                            f"Capability assessment: Unchanged: $(touch {sentinel}) "
                            f"`touch {sentinel}` ::error:: <!-- are inert PR text.\n" + body
                        )
                    result = self.run_event(body, user={"login": author}, title=title)
                    self.assertEqual(result.returncode == 0, assessed, result.stderr)
                    self.assertFalse(sentinel.exists())
                    self.assertNotIn(str(sentinel), result.stdout + result.stderr)
                    self.assertNotIn("::error::", result.stdout if assessed else "")


if __name__ == "__main__":
    unittest.main()
