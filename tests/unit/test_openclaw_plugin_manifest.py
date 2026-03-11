import json
from pathlib import Path


def _parse_plugin_yaml(path: Path) -> dict[str, object]:
    data: dict[str, object] = {}
    capabilities: list[str] = []
    runtime: dict[str, str] = {}
    section = ""

    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.rstrip()
        stripped = line.strip()

        if not stripped or stripped.startswith("#"):
            continue

        if stripped == "capabilities:":
            section = "capabilities"
            continue

        if stripped == "runtime:":
            section = "runtime"
            continue

        if section == "capabilities" and stripped.startswith("- "):
            capabilities.append(stripped[2:].strip())
            continue

        if section == "runtime" and line.startswith("  ") and ":" in stripped:
            key, value = stripped.split(":", 1)
            runtime[key.strip()] = value.strip().strip("\"'")
            continue

        if ":" in stripped and not line.startswith(" "):
            key, value = stripped.split(":", 1)
            data[key.strip()] = value.strip().strip("\"'")
            section = ""

    data["capabilities"] = capabilities
    data["runtime"] = runtime
    return data


def test_manifests_and_package_json_share_plugin_identity():
    repo_root = Path(__file__).resolve().parents[2]
    plugin_dir = repo_root / "plugins" / "openclaw-memory"

    yaml_data = _parse_plugin_yaml(plugin_dir / "plugin.yaml")
    json_data = json.loads((plugin_dir / "openclaw.plugin.json").read_text(encoding="utf-8"))
    package_data = json.loads((plugin_dir / "package.json").read_text(encoding="utf-8"))

    assert json_data["id"] == yaml_data["slug"] == package_data["name"]
    assert json_data["name"] == yaml_data["name"]
    assert json_data["version"] == yaml_data["version"] == package_data["version"]
    assert set(json_data["capabilities"]) == set(yaml_data["capabilities"])
    assert package_data["openclaw"]["extensions"] == ["./src/index.ts"]


def test_openclaw_plugin_json_declares_required_schema_tools_and_hooks():
    repo_root = Path(__file__).resolve().parents[2]
    plugin_dir = repo_root / "plugins" / "openclaw-memory"
    data = json.loads((plugin_dir / "openclaw.plugin.json").read_text(encoding="utf-8"))

    assert set(data) >= {"id", "name", "version", "description", "configSchema"}
    assert set(data["configSchema"]["properties"]) >= {
        "autoRecall",
        "autoCapture",
        "embeddingProvider",
        "pythonExecutable",
        "runtimeRoot",
    }
    assert [tool["name"] for tool in data["tools"]] == [
        "memory_recall",
        "memory_store",
        "memory_forget",
        "memory_update",
    ]
    assert [hook["name"] for hook in data["hooks"]] == [
        "before_prompt_build",
        "before_agent_start",
        "agent_end",
    ]
