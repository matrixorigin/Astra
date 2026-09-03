#!/usr/bin/env python3
"""Repository metadata, documentation, and deterministic setup checks."""

from __future__ import annotations

from pathlib import Path
import json
import re
import stat
import subprocess


def tracked_files() -> list[Path]:
    output = subprocess.check_output(["git", "ls-files", "-z"])
    return [Path(item.decode("utf-8")) for item in output.split(b"\0") if item]


def skill_body(path: Path) -> str:
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---\n"):
        raise AssertionError(f"{path}: missing YAML frontmatter")
    _, separator, body = text[4:].partition("\n---\n")
    if not separator:
        raise AssertionError(f"{path}: unterminated YAML frontmatter")
    return body


def main() -> None:
    files = [path for path in tracked_files() if path.exists()]
    errors: list[str] = []

    markdown = [path for path in files if path.suffix in {".md", ".mdc"}]
    link_pattern = re.compile(r"(?<!!)\[[^]]*\]\(([^)\s]+)(?:\s+[\"'][^\"']*[\"'])?\)")
    for source in markdown:
        text = source.read_text(encoding="utf-8", errors="replace")
        for target in link_pattern.findall(text):
            target = target.strip("<>")
            if target.startswith(("http://", "https://", "#", "mailto:", "data:")):
                continue
            relative = target.split("#", 1)[0]
            if relative and not (source.parent / relative).resolve().exists():
                errors.append(f"{source}: broken local link {target}")

    parsed_json: dict[Path, object] = {}
    for source in [path for path in files if path.suffix == ".json"]:
        try:
            parsed_json[source] = json.loads(source.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            errors.append(f"{source}: invalid JSON ({error})")

    shell_scripts = [path for path in files if path.suffix == ".sh"]
    for source in shell_scripts:
        result = subprocess.run(
            ["bash", "-n", str(source)],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode:
            errors.append(f"{source}: invalid shell syntax ({result.stderr.strip()})")

    contract_scripts = [
        Path("scripts/dev/test_setup_contract.sh"),
        Path("scripts/dev/test_edge_process_contract.sh"),
        Path("scripts/ops/test_production_env_contract.sh"),
        Path("scripts/ci/test_release_contract.sh"),
        Path("scripts/ci/test_sccache_fallback.sh"),
    ]
    for contract_script in contract_scripts:
        result = subprocess.run(
            [str(contract_script)],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            errors.append(f"{contract_script}: contract failed ({detail})")

    workflow_files = [
        *Path(".github/workflows").glob("*.yml"),
        *Path(".github/workflows").glob("*.yaml"),
        *Path(".github/actions").glob("*/action.yml"),
        *Path(".github/actions").glob("*/action.yaml"),
    ]
    uses_pattern = re.compile(r"^\s*-?\s*uses:\s*([^\s#]+)", re.MULTILINE)
    pinned_action = re.compile(r"^[^@]+@[0-9a-f]{40}$")
    for source in workflow_files:
        text = source.read_text(encoding="utf-8")
        for action in uses_pattern.findall(text):
            if action.startswith(("./", "docker://")):
                continue
            if not pinned_action.fullmatch(action):
                errors.append(f"{source}: action must be pinned to a full commit SHA ({action})")

    release_workflow = Path(".github/workflows/release-docker.yml").read_text(encoding="utf-8")
    build_start = release_workflow.find("\n  build:")
    smoke_start = release_workflow.find("\n  smoke:")
    merge_start = release_workflow.find("\n  merge:")
    promote_start = release_workflow.find("\n  promote:")
    mirror_start = release_workflow.find("\n  registry-mirror:")
    if not 0 <= build_start < smoke_start < merge_start < promote_start < mirror_start:
        errors.append(
            ".github/workflows/release-docker.yml: release jobs must smoke untagged digests, "
            "publish the verified manifest, and only then promote rolling tags"
        )
    else:
        build_job = release_workflow[build_start:smoke_start]
        smoke_job = release_workflow[smoke_start:merge_start]
        merge_job = release_workflow[merge_start:promote_start]
        promote_job = release_workflow[promote_start:mirror_start]
        if "push-by-digest=true" not in build_job or "name-canonical=true" not in build_job:
            errors.append(
                ".github/workflows/release-docker.yml: candidate builds must be pushed by digest without a tag"
            )
        if "type=raw,value=" in smoke_job or "- merge" in smoke_job:
            errors.append(
                ".github/workflows/release-docker.yml: smoke job must verify build digests before any tag is published"
            )
        if "- build" not in smoke_job or "@sha256:" not in smoke_job or "make stack-verify" not in smoke_job:
            errors.append(
                ".github/workflows/release-docker.yml: candidate digests must be verified through the all-in-one runtime"
            )
        if (
            "- smoke" not in merge_job
            or "type=raw,value=latest" in merge_job
            or "actual_platforms" not in merge_job
            or "expected_platforms" not in merge_job
        ):
            errors.append(
                ".github/workflows/release-docker.yml: version manifest must depend on smoke and validate its platform set"
            )
        if (
            "- merge" not in promote_job
            or '--tag "${IMAGE_NAME}:latest"' not in promote_job
            or "expected_platforms" not in promote_job
            or "actual_platforms" not in promote_job
        ):
            errors.append(
                ".github/workflows/release-docker.yml: rolling promotion must depend on and match the verified manifest"
            )

    binary_release_workflow = Path(".github/workflows/release-binaries.yml").read_text(
        encoding="utf-8"
    )
    for forbidden in (
        "ASTRA_SUITE_PAT",
        "RELEASE_MIRROR_REPOSITORY",
        "repository: ${{",
        "\n  mirror:",
    ):
        if forbidden in binary_release_workflow:
            errors.append(
                ".github/workflows/release-binaries.yml: releases must remain owned by the current repository "
                f"(found {forbidden.strip()})"
            )
    for required in (
        "permissions:\n      contents: write",
        "fetch-depth: 0",
        "git merge-base --is-ancestor",
        "EVENT_SOURCE_SHA",
        "source_sha",
        "astra-edge",
        "scripts/verify-release-artifacts.sh",
        "fail_on_unmatched_files: true",
    ):
        if required not in binary_release_workflow:
            errors.append(
                ".github/workflows/release-binaries.yml: missing current-repository release guard "
                f"({required})"
            )

    for required in (
        "fetch-depth: 0",
        "EVENT_SOURCE_SHA",
        "source_sha",
        "EXPECTED_TAG_OBJECT",
    ):
        if required not in release_workflow:
            errors.append(
                ".github/workflows/release-docker.yml: missing immutable release source guard "
                f"({required})"
            )

    installer = Path("scripts/install-astra.sh").read_text(encoding="utf-8")
    if 'REPOSITORY="matrixorigin/Astra"' not in installer:
        errors.append("scripts/install-astra.sh: installer must download from matrixorigin/Astra")
    if (
        "failed to download the required checksum" not in installer
        or "checksum mismatch" not in installer
    ):
        errors.append(
            "scripts/install-astra.sh: release checksum verification must be mandatory"
        )

    release_version_validator = Path("scripts/validate-release-version.sh").read_text(
        encoding="utf-8"
    )
    for version_source in (
        "packages/sdk/package.json",
        "web/package.json",
        "CITATION.cff",
        "deployment/kubernetes/chart/Chart.yaml",
    ):
        if version_source not in release_version_validator:
            errors.append(
                f"scripts/validate-release-version.sh: missing release version source {version_source}"
            )

    makefile = Path("Makefile").read_text(encoding="utf-8")
    for required in (
        "stack-start: stack-env",
        "$(MAKE) stack-up",
        "$(MAKE) stack-verify",
        "dev-start: dev-start-server-only",
    ):
        if required not in makefile:
            errors.append(f"Makefile: missing reproducible local journey contract ({required})")

    edge_lifecycle = "\n".join(
        Path(path).read_text(encoding="utf-8")
        for path in ("scripts/dev/start-edge.sh", "scripts/dev/stop-edge.sh")
    )
    if "pgrep" in edge_lifecycle or "edge_process_is_owned" not in edge_lifecycle:
        errors.append(
            "scripts/dev: edge lifecycle must manage only the PID owned by this checkout"
        )

    design_index = Path("docs/design/README.md").read_text(encoding="utf-8")
    for design in Path("docs/design").glob("*.md"):
        if design.name != "README.md" and f"]({design.name})" not in design_index:
            errors.append(f"{design}: missing from docs/design/README.md")

    for adapter in [
        Path("CLAUDE.md"),
        Path(".claude/CLAUDE.md"),
        Path(".cursor/rules/project-rules.mdc"),
        Path(".kiro/steering/project-rules.md"),
    ]:
        text = adapter.read_text(encoding="utf-8")
        if "AGENTS.md" not in text or "canonical" not in text:
            errors.append(f"{adapter}: must delegate to canonical AGENTS.md")

    agent_root = Path(".agent/skills")
    claude_root = Path(".claude/skills")
    agent_names = {path.parent.name for path in agent_root.glob("*/SKILL.md")}
    claude_names = {path.parent.name for path in claude_root.glob("*/SKILL.md")}
    if agent_names != claude_names:
        errors.append(".agent/skills and .claude/skills expose different skill sets")
    for name in sorted(agent_names & claude_names):
        if skill_body(agent_root / name / "SKILL.md") != skill_body(claude_root / name / "SKILL.md"):
            errors.append(f"{name}: .agent and .claude instruction bodies differ")

    stale_suffixes = {".bak", ".disabled", ".orig", ".rej"}
    generated_parts = {
        ".next",
        ".pytest_cache",
        ".turbo",
        "__pycache__",
        "coverage",
        "dist",
        "node_modules",
        "target",
    }
    for path in files:
        if path.suffix in stale_suffixes:
            errors.append(f"{path}: tracked stale/disabled artifact")
        if generated_parts.intersection(path.parts):
            errors.append(f"{path}: tracked generated/cache artifact")
        if path.name in {".DS_Store", "Thumbs.db"}:
            errors.append(f"{path}: tracked operating-system artifact")
        if (path.name == ".env" or path.name.startswith(".env.")) and not path.name.endswith(
            ".example"
        ):
            errors.append(f"{path}: tracked environment file; commit a sanitized example instead")

        try:
            has_shebang = path.read_bytes().startswith(b"#!")
            executable = bool(path.stat().st_mode & stat.S_IXUSR)
        except OSError as error:
            errors.append(f"{path}: cannot inspect file mode contract ({error})")
            continue
        if executable and not has_shebang:
            errors.append(f"{path}: executable file is missing a shebang")
        if path.parts and path.parts[0] == "scripts" and has_shebang and not executable:
            errors.append(f"{path}: script has a shebang but is not executable")

    rust_source = "\n".join(
        path.read_text(encoding="utf-8", errors="replace")
        for path in files
        if path.parts
        and path.parts[0] == "crates"
        and path.suffix == ".rs"
        and "tests" not in path.parts
        and path.name not in {"test.rs", "tests.rs"}
    )
    exported_metrics = set(re.findall(r"\bastra_[a-z0-9_]+\b", rust_source))
    latency_prefixes = re.findall(
        r'register_latency_metrics\(\s*[^,]+,\s*"(astra_[a-z0-9_]+)"', rust_source
    )
    for prefix in latency_prefixes:
        exported_metrics.update(
            f"{prefix}{suffix}"
            for suffix in ("_us_total", "_count", "_min_us", "_max_us", "_avg_us")
        )

    monitoring_sources = [
        path
        for path in files
        if path == Path("monitoring/alert-rules.yml")
        or path.parent == Path("deployment/monitoring/dashboards")
    ]
    for source in monitoring_sources:
        if source.parent == Path("deployment/monitoring/dashboards"):
            dashboard = parsed_json.get(source)
            if not isinstance(dashboard, dict):
                continue
            if "dashboard" in dashboard:
                errors.append(
                    f"{source}: file-provisioned dashboard must not use the HTTP API wrapper"
                )
            if not isinstance(dashboard.get("title"), str) or not isinstance(
                dashboard.get("panels"), list
            ):
                errors.append(f"{source}: invalid file-provisioned dashboard shape")
        referenced_metrics = set(
            re.findall(
                r"\bastra_[a-z0-9_]+\b",
                source.read_text(encoding="utf-8", errors="replace"),
            )
        )
        for metric in sorted(referenced_metrics - exported_metrics):
            errors.append(f"{source}: references metric not found in Rust sources ({metric})")

    if errors:
        raise SystemExit("\n".join(errors))
    print(
        f"repository metadata: ok ({len(markdown)} Markdown/rule files, "
        f"{len(shell_scripts)} shell scripts, {len(agent_names)} mirrored skills, "
        f"{len(monitoring_sources)} monitoring definitions)"
    )


if __name__ == "__main__":
    main()
