#!/usr/bin/env python3
"""Dependency-free repository metadata and documentation checks."""

from __future__ import annotations

from pathlib import Path
import json
import re
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

    for source in [path for path in files if path.suffix == ".json"]:
        try:
            json.loads(source.read_text(encoding="utf-8"))
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
    for path in files:
        if path.suffix in stale_suffixes:
            errors.append(f"{path}: tracked stale/disabled artifact")

    if errors:
        raise SystemExit("\n".join(errors))
    print(
        f"repository metadata: ok ({len(markdown)} Markdown/rule files, "
        f"{len(shell_scripts)} shell scripts, {len(agent_names)} mirrored skills)"
    )


if __name__ == "__main__":
    main()
