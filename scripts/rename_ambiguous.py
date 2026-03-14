#!/usr/bin/env python3
"""Context-aware rename of ambiguous table names in SQL strings.

Only renames table names when they appear in SQL contexts:
- FROM/JOIN/INTO/UPDATE/TABLE <name>
- DELETE FROM <name>
- text("...SELECT...FROM <name>...")
"""

import os
import re

# Ambiguous names: old -> new
AMBIGUOUS = {
    "users": "auth_users",
    "roles": "auth_roles",
    "user_roles": "auth_user_roles",
    "refresh_tokens": "auth_refresh_tokens",
    "tokens": "auth_tokens",
    "agents": "agent_agents",
    "sessions": "agent_sessions",
    "triggers": "wf_triggers",
    "configs": "infra_configs",
    "repos": "infra_repos",
    "memories": "mem_memories",
}

SKIP_DIRS = {"__pycache__", ".git", ".mypy_cache", ".ruff_cache", "node_modules"}

# SQL keywords that precede table names
SQL_KW = r"(?:FROM|JOIN|INTO|UPDATE|TABLE|DELETE\s+FROM)"


def build_pattern():
    """Build regex that matches SQL keyword + old table name."""
    names = "|".join(re.escape(n) for n in AMBIGUOUS)
    # Match: SQL_KEYWORD whitespace TABLE_NAME (with word boundary)
    return re.compile(rf"({SQL_KW})\s+({names})\b", re.IGNORECASE)


PAT = build_pattern()


def replace_match(m):
    kw = m.group(1)
    old_name = m.group(2)
    # Preserve original case of keyword, replace table name
    # Look up case-insensitively
    for old, new in AMBIGUOUS.items():
        if old_name.lower() == old.lower():
            return f"{kw} {new}"
    return m.group(0)


def should_process(rel_path):
    if rel_path.startswith("api/models/"):
        return False
    prefixes = ["core/", "api/", "cli/", "scripts/", "skills/", "tests/", "examples/"]
    return any(rel_path.startswith(p) for p in prefixes)


count = 0
for root, dirs, files in os.walk("."):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for f in files:
        if not f.endswith(".py"):
            continue
        path = os.path.join(root, f)
        rel = path[2:]
        if not should_process(rel):
            continue

        with open(path) as fh:
            content = fh.read()

        new_content = PAT.sub(replace_match, content)

        if new_content != content:
            with open(path, "w") as fh:
                fh.write(new_content)
            count += 1
            # Show what changed
            for i, (old_line, new_line) in enumerate(
                zip(content.splitlines(), new_content.splitlines())
            ):
                if old_line != new_line:
                    print(f"  {rel}:{i + 1}: {old_line.strip()} -> {new_line.strip()}")

print(f"\nUpdated {count} files")
