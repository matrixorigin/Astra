import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


def test_openclaw_plugin_entrypoint_is_self_contained(tmp_path):
    repo_root = Path(__file__).resolve().parents[2]
    source_package = repo_root / "plugins" / "openclaw-memory"
    isolated_package = tmp_path / "openclaw-memory"
    shutil.copytree(source_package, isolated_package)

    script = """
import runpy
from dataclasses import dataclass

module = runpy.run_path("src/openclaw_memory_plugin.py")
Plugin = module["OpenClawMemoryPlugin"]

@dataclass
class FakeContext:
    selected_events: list[dict]
    def to_prompt(self):
        return "assembled prompt"

class FakeContextManager:
    def __init__(self):
        self.calls = []
    def build_context(self, session_id, query, max_tokens=4000, task_type="general"):
        self.calls.append(task_type)
        return FakeContext(selected_events=[{
            "event_id": "evt-1",
            "event_type": "user_message",
            "content": "remember this",
            "score": 0.9,
        }])

plugin = Plugin(FakeContextManager())
snippets = plugin.retrieve_relevant_memory(session_id="s-1", query="q", task_type="PLANNING")
assert len(snippets) == 1
assert snippets[0].event_id == "evt-1"
assert plugin.build_context_prompt(session_id="s-1", query="q", task_type="unknown") == "assembled prompt"
print("ok")
"""

    env = os.environ.copy()
    env["PYTHONPATH"] = str(isolated_package)
    result = subprocess.run(
        [sys.executable, "-c", script],
        cwd=isolated_package,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode == 0, (
        "Plugin entrypoint failed in isolated package context.\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}\n"
    )
    assert "ok" in result.stdout


def test_package_json_extensions_target_existing_package_files():
    repo_root = Path(__file__).resolve().parents[2]
    plugin_dir = repo_root / "plugins" / "openclaw-memory"
    package_data = json.loads((plugin_dir / "package.json").read_text(encoding="utf-8"))

    extensions = package_data["openclaw"]["extensions"]
    for relative_path in extensions:
        assert (plugin_dir / relative_path).exists()

    assert (plugin_dir / "src" / "backend_bridge.py").exists()
