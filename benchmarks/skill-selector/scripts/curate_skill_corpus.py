#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path
from typing import Any

import yaml


REPO_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_BASE_DIR = Path(
    os.environ.get("ASTRA_SKILL_SELECTOR_BENCH_BASE", REPO_ROOT / "tmp" / "selector-skill-libraries")
)

BASE_DIR = DEFAULT_BASE_DIR
CURATED_DIR = BASE_DIR / "astra-curated-skills"
QUARANTINE_DIR = BASE_DIR / "astra-quarantine"
SAMPLE_DIR = BASE_DIR / "astra-benchmark-1000"
REPORT_PATH = BASE_DIR / "astra-curation-report.json"


SOURCE_PRIORITY = {
    "skills": 0,
    "stitch-skills": 1,
    "claude-skills": 2,
    "antigravity-awesome-skills": 3,
}

OFFENSIVE_SECURITY_RE = re.compile(
    r"\b("
    r"metasploit|credential[- ]?theft|token[- ]?steal|penetration[- ]?testing|"
    r"reverse[- ]?shell|keylogger|phishing|sql[- ]?injection|xss|csrf|"
    r"privilege[- ]?escalation|lateral[- ]?movement|malware|ransomware|"
    r"command[- ]?and[- ]?control|red[- ]?team|brute[- ]?force|"
    r"exploit(?:ation)?|payloads?"
    r")\b",
    re.I,
)

SUSPICIOUS_CONTENT_RULES = {
    "ignore_system_instructions": re.compile(
        r"ignore (all )?(previous|prior|system) instructions", re.I
    ),
    "destructive_git": re.compile(r"git\s+(reset --hard|clean -fd|checkout --)", re.I),
    "dangerous_rm_rf": re.compile(r"rm\s+-rf\s+(/|~|\.($|\s)|\$\w+)", re.I),
    "shell_eval_obfuscation": re.compile(r"\$\{[^}]+@P\}|\$\{!\w+\}", re.I),
    "explicit_secret_exfiltration": re.compile(
        r"(send|upload|post|exfiltrat)[^\n]{0,200}(\$[A-Z0-9_]*"
        r"(TOKEN|KEY|SECRET|PASSWORD|COOKIE|SSH)[A-Z0-9_]*|Authorization:|Bearer\s+\$\{?[A-Z0-9_]+)",
        re.I,
    ),
}


def slugify(value: str) -> str:
    value = value.strip().lower()
    value = re.sub(r"[^a-z0-9]+", "-", value)
    value = re.sub(r"-{2,}", "-", value).strip("-")
    if not value:
        value = "imported-skill"
    return value[:128]


def first_meaningful_paragraph(body: str) -> str:
    blocks: list[str] = []
    current: list[str] = []
    for line in body.splitlines():
        stripped = line.strip()
        if not stripped:
            if current:
                blocks.append(" ".join(current).strip())
                current = []
            continue
        if stripped.startswith("#") or stripped.startswith("```"):
            if current:
                blocks.append(" ".join(current).strip())
                current = []
            continue
        if stripped.startswith("**Name**") or stripped.startswith("**Tier**"):
            continue
        current.append(stripped)
    if current:
        blocks.append(" ".join(current).strip())
    for block in blocks:
        if len(block) >= 24:
            return block[:320]
    return "Imported community skill for Astra selector benchmarking."


def extract_description_from_markdown(body: str) -> str:
    match = re.search(r"^##\s+Description\s*$([\s\S]+?)(?:^\s*##\s+|\Z)", body, re.M)
    if match:
        text = first_meaningful_paragraph(match.group(1))
        if text:
            return text
    return first_meaningful_paragraph(body)


def split_frontmatter_candidates(text: str) -> list[tuple[dict[str, Any], str]]:
    lines = text.splitlines()
    markers = [idx for idx, line in enumerate(lines) if line.strip() == "---"]
    candidates: list[tuple[dict[str, Any], str]] = []
    for start_idx in range(len(markers) - 1):
        start = markers[start_idx]
        for end in markers[start_idx + 1 :]:
            if end <= start:
                continue
            yaml_text = "\n".join(lines[start + 1 : end])
            if not yaml_text.strip():
                continue
            try:
                parsed = yaml.safe_load(yaml_text)
            except Exception:
                continue
            if isinstance(parsed, dict):
                body = "\n".join(lines[end + 1 :]).lstrip("\n")
                candidates.append((parsed, body))
    return candidates


@dataclass
class SkillEntry:
    source_path: Path
    skill_dir: Path
    repo_name: str
    canonical_name: str
    frontmatter: dict[str, Any]
    body: str
    raw_text: str
    parse_mode: str
    description: str
    content_hash: str
    suspicious_reasons: list[str]
    source_priority: int
    plugin_penalty: int


def repo_name_for(path: Path) -> str:
    rel = path.relative_to(BASE_DIR)
    return rel.parts[0]


def priority_for(path: Path) -> tuple[int, int]:
    repo = repo_name_for(path)
    priority = SOURCE_PRIORITY.get(repo, 99)
    plugin_penalty = 1 if "plugins" in path.parts or ".gemini" in path.parts else 0
    return priority, plugin_penalty


def suspicious_reasons_for(path: Path, text: str, frontmatter: dict[str, Any]) -> list[str]:
    reasons: list[str] = []
    name = str(frontmatter.get("name") or path.parent.name)
    haystack = f"{path}\n{name}"
    if OFFENSIVE_SECURITY_RE.search(haystack):
        reasons.append("offensive-security")
    for reason, pattern in SUSPICIOUS_CONTENT_RULES.items():
        if pattern.search(text):
            reasons.append(reason)
    return reasons


def load_entry(path: Path) -> SkillEntry | None:
    text = path.read_text(encoding="utf-8", errors="ignore")
    candidates = split_frontmatter_candidates(text)
    if candidates:
        preferred = None
        for frontmatter, body in candidates:
            name = str(frontmatter.get("name") or "").strip()
            description = str(frontmatter.get("description") or "").strip()
            if name or description:
                preferred = (frontmatter, body, "recovered")
                break
        if preferred is None:
            preferred = (*candidates[0], "recovered")
        frontmatter, body, parse_mode = preferred
    else:
        frontmatter, body, parse_mode = {}, text, "synthesized"

    name = str(frontmatter.get("name") or "").strip()
    if not name:
        name = path.parent.name
    canonical_name = slugify(name)

    description = str(frontmatter.get("description") or "").strip()
    if not description:
        description = extract_description_from_markdown(body)

    if not canonical_name:
        return None

    priority, plugin_penalty = priority_for(path)
    return SkillEntry(
        source_path=path,
        skill_dir=path.parent,
        repo_name=repo_name_for(path),
        canonical_name=canonical_name,
        frontmatter=dict(frontmatter),
        body=body.strip("\n") + "\n",
        raw_text=text,
        parse_mode=parse_mode,
        description=description,
        content_hash=sha256(text.encode("utf-8", errors="ignore")).hexdigest(),
        suspicious_reasons=suspicious_reasons_for(path, text, frontmatter),
        source_priority=priority,
        plugin_penalty=plugin_penalty,
    )


def selection_key(entry: SkillEntry) -> tuple[int, int, int, int, int, str]:
    return (
        entry.source_priority,
        entry.plugin_penalty,
        0 if entry.parse_mode == "recovered" else 1,
        0 if entry.description else 1,
        -len(entry.description),
        str(entry.source_path),
    )


def yaml_dump(frontmatter: dict[str, Any]) -> str:
    return yaml.safe_dump(frontmatter, sort_keys=False, allow_unicode=True).strip()


def normalize_string_list(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(item).strip() for item in value if str(item).strip()]
    if isinstance(value, str):
        text = value.strip()
        if not text:
            return []
        if text.startswith("[") and text.endswith("]"):
            try:
                parsed = yaml.safe_load(text)
                if isinstance(parsed, list):
                    return [str(item).strip() for item in parsed if str(item).strip()]
            except Exception:
                pass
        if "," in text:
            return [part.strip() for part in text.split(",") if part.strip()]
        return [text]
    return [str(value).strip()]


def normalize_frontmatter(frontmatter: dict[str, Any]) -> dict[str, Any]:
    normalized = dict(frontmatter)
    for key in ("tags", "aliases", "triggers", "allowed_tools", "paths", "required_capabilities"):
        if key in normalized:
            normalized[key] = normalize_string_list(normalized.get(key))
    if "depends_on" in normalized and isinstance(normalized["depends_on"], str):
        normalized["depends_on"] = [normalized["depends_on"]]
    if "compatibility" in normalized and not isinstance(normalized["compatibility"], dict):
        normalized.pop("compatibility", None)
    if "publisher" in normalized and not isinstance(normalized["publisher"], dict):
        normalized.pop("publisher", None)
    if "hooks" in normalized and not isinstance(normalized["hooks"], dict):
        normalized.pop("hooks", None)
    if "composition" in normalized and not isinstance(normalized["composition"], dict):
        normalized.pop("composition", None)
    for key in ("user_invocable", "isolated"):
        value = normalized.get(key)
        if isinstance(value, str):
            lowered = value.strip().lower()
            if lowered in {"true", "false"}:
                normalized[key] = lowered == "true"
    if "max_tokens" in normalized and isinstance(normalized["max_tokens"], str):
        try:
            normalized["max_tokens"] = int(normalized["max_tokens"].strip())
        except ValueError:
            normalized.pop("max_tokens", None)
    return normalized


def write_skill(entry: SkillEntry, target_dir: Path) -> None:
    if target_dir.exists():
        shutil.rmtree(target_dir)
    shutil.copytree(entry.skill_dir, target_dir)
    skill_md = target_dir / "SKILL.md"
    frontmatter = normalize_frontmatter(entry.frontmatter)
    frontmatter["name"] = entry.canonical_name
    frontmatter["description"] = entry.description
    skill_md.write_text(f"---\n{yaml_dump(frontmatter)}\n---\n\n{entry.body}", encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Curate the skill-selector benchmark corpus.")
    parser.add_argument("--base-dir", default=str(DEFAULT_BASE_DIR))
    return parser.parse_args()


def configure_paths(base_dir: Path) -> None:
    global BASE_DIR, CURATED_DIR, QUARANTINE_DIR, SAMPLE_DIR, REPORT_PATH
    BASE_DIR = base_dir
    CURATED_DIR = BASE_DIR / "astra-curated-skills"
    QUARANTINE_DIR = BASE_DIR / "astra-quarantine"
    SAMPLE_DIR = BASE_DIR / "astra-benchmark-1000"
    REPORT_PATH = BASE_DIR / "astra-curation-report.json"


def main() -> None:
    args = parse_args()
    configure_paths(Path(args.base_dir))
    raw_entries = [entry for entry in (load_entry(path) for path in BASE_DIR.rglob("SKILL.md")) if entry]

    # Drop our own generated outputs on reruns.
    raw_entries = [
        entry
        for entry in raw_entries
        if entry.repo_name not in {"astra-curated-skills", "astra-quarantine", "astra-benchmark-1000"}
    ]

    by_content: dict[str, SkillEntry] = {}
    for entry in sorted(raw_entries, key=selection_key):
        by_content.setdefault(entry.content_hash, entry)
    content_dedup = list(by_content.values())

    by_name: dict[str, SkillEntry] = {}
    for entry in sorted(content_dedup, key=selection_key):
        by_name.setdefault(entry.canonical_name, entry)
    selected = list(by_name.values())

    CURATED_DIR.mkdir(parents=True, exist_ok=True)
    QUARANTINE_DIR.mkdir(parents=True, exist_ok=True)
    SAMPLE_DIR.mkdir(parents=True, exist_ok=True)

    for directory in (CURATED_DIR, QUARANTINE_DIR, SAMPLE_DIR):
        for child in directory.iterdir():
            if child.is_symlink() or child.is_file():
                child.unlink()
            else:
                shutil.rmtree(child)

    curated: list[SkillEntry] = []
    quarantined: list[dict[str, Any]] = []

    for entry in sorted(selected, key=selection_key):
        if entry.suspicious_reasons:
            quarantine_target = QUARANTINE_DIR / entry.canonical_name
            write_skill(entry, quarantine_target)
            quarantined.append(
                {
                    "name": entry.canonical_name,
                    "source_path": str(entry.source_path),
                    "repo": entry.repo_name,
                    "reasons": entry.suspicious_reasons,
                }
            )
            continue
        curated_target = CURATED_DIR / entry.canonical_name
        write_skill(entry, curated_target)
        curated.append(entry)

    benchmark_sample = sorted(curated, key=selection_key)[:1000]
    for entry in benchmark_sample:
        src = CURATED_DIR / entry.canonical_name
        dst = SAMPLE_DIR / entry.canonical_name
        dst.symlink_to(src, target_is_directory=True)

    report = {
        "base_dir": str(BASE_DIR),
        "curated_dir": str(CURATED_DIR),
        "quarantine_dir": str(QUARANTINE_DIR),
        "sample_dir": str(SAMPLE_DIR),
        "raw_skill_md_count": len(raw_entries),
        "after_content_dedup": len(content_dedup),
        "after_name_dedup": len(selected),
        "curated_count": len(curated),
        "quarantined_count": len(quarantined),
        "benchmark_sample_count": len(benchmark_sample),
        "curated_parse_mode_breakdown": {
            mode: sum(1 for entry in curated if entry.parse_mode == mode)
            for mode in sorted({entry.parse_mode for entry in curated})
        },
        "quarantine_reason_breakdown": {
            reason: sum(1 for item in quarantined if reason in item["reasons"])
            for reason in sorted({reason for item in quarantined for reason in item["reasons"]})
        },
        "quarantine_examples": quarantined[:50],
        "sample_names": [entry.canonical_name for entry in benchmark_sample[:100]],
    }
    REPORT_PATH.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
