"""mo-memory CLI — configure AI tools to use shared memory service.

Usage:
    mo-memory init       # Detect tools, write MCP config + steering rules
    mo-memory status     # Show connection status
    mo-memory health     # Health check
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any

# ── MCP config templates ──────────────────────────────────────────────


def _mcp_config(mode: str = "stdio", db_url: str | None = None, **embed_opts: str) -> dict:
    import sys

    if mode == "remote":
        return {"url": "http://localhost:8100/mcp"}
    cfg: dict[str, Any] = {
        "command": sys.executable,
        "args": ["-m", "mo_memory_mcp"],
    }
    env: dict[str, str] = {}
    if db_url:
        env["MO_MEMORY_DB_URL"] = db_url
    for key in ("provider", "model", "dim", "api_key", "base_url"):
        val = embed_opts.get(key)
        if val:
            env[f"MO_MEMORY_EMBEDDING_{key.upper()}"] = val
    if env:
        cfg["env"] = env
    return cfg


# ── Steering rule content ─────────────────────────────────────────────

_KIRO_STEERING = """\
---
inclusion: always
---

# Memory Integration

You have access to a shared memory service via MCP tools. Use it proactively:

## When to store memories
- User states a preference, fact, or decision → `memory_store`
- User corrects you → `memory_store` the correction as a fact
- Important project context is shared → `memory_store` with type "fact"

## When to retrieve memories
- At the START of every conversation → `memory_retrieve` with a summary of the user's first message
- When the user references something from a past conversation → `memory_retrieve`
- Before making assumptions about preferences → `memory_retrieve`

## When to correct/purge
- User says a stored memory is wrong → `memory_correct`
- User asks to forget something → `memory_purge`

## Memory types
- `profile`: user/agent profiles
- `semantic`: project facts, technical decisions, architecture choices
- `procedural`: how-to knowledge, workflows, processes
- `working`: temporary context for current task
- `tool_result`: results from tool executions
"""

_CURSOR_RULE = """\
# Memory Integration

You have access to a shared memory service via MCP tools (mo-memory).

**Always retrieve memories** at the start of conversations using `memory_retrieve`.
**Store important information** using `memory_store` when the user shares facts, preferences, or decisions.
**Correct memories** with `memory_correct` when information is outdated.
**Purge memories** with `memory_purge` when asked to forget something.

Memory types: profile, semantic, procedural, working, tool_result.
"""

_CLAUDE_RULE = """\

## Memory Integration

This project uses mo-memory for shared memory across AI tools.
MCP tools available: memory_store, memory_retrieve, memory_correct, memory_purge, memory_profile, memory_search.

- Retrieve memories at conversation start with `memory_retrieve`
- Store facts/preferences/decisions with `memory_store`
- Correct outdated info with `memory_correct`
- Delete on request with `memory_purge`

Memory types: profile, semantic, procedural, working, tool_result.
"""


# ── Detection & writing ───────────────────────────────────────────────

def _detect_tools(project_dir: Path) -> dict[str, bool]:
    return {
        "kiro": (project_dir / ".kiro").is_dir(),
        "cursor": (project_dir / ".cursor").is_dir() or (project_dir / ".cursorrc").exists(),
        "claude": (project_dir / "CLAUDE.md").exists() or (project_dir / ".claude").is_dir(),
    }


def _write_kiro(project_dir: Path, mode: str, db_url: str | None = None, **embed_opts: str) -> list[str]:
    actions = []

    # MCP config
    mcp_dir = project_dir / ".kiro" / "settings"
    mcp_dir.mkdir(parents=True, exist_ok=True)
    mcp_file = mcp_dir / "mcp.json"

    if mcp_file.exists():
        config = json.loads(mcp_file.read_text())
    else:
        config = {"mcpServers": {}}

    config.setdefault("mcpServers", {})
    config["mcpServers"]["mo-memory"] = _mcp_config(mode, db_url, **embed_opts)
    mcp_file.write_text(json.dumps(config, indent=2) + "\n")
    actions.append(f"  ✅ {mcp_file.relative_to(project_dir)}")

    # Steering rule
    steering_dir = project_dir / ".kiro" / "steering"
    steering_dir.mkdir(parents=True, exist_ok=True)
    rule_file = steering_dir / "memory.md"
    rule_file.write_text(_KIRO_STEERING)
    actions.append(f"  ✅ {rule_file.relative_to(project_dir)}")

    return actions


def _write_cursor(project_dir: Path, mode: str, db_url: str | None = None, **embed_opts: str) -> list[str]:
    actions = []

    # MCP config
    cursor_dir = project_dir / ".cursor"
    cursor_dir.mkdir(parents=True, exist_ok=True)
    mcp_file = cursor_dir / "mcp.json"

    if mcp_file.exists():
        config = json.loads(mcp_file.read_text())
    else:
        config = {"mcpServers": {}}

    config.setdefault("mcpServers", {})
    config["mcpServers"]["mo-memory"] = _mcp_config(mode, db_url, **embed_opts)
    mcp_file.write_text(json.dumps(config, indent=2) + "\n")
    actions.append(f"  ✅ {mcp_file.relative_to(project_dir)}")

    # Rule file
    rules_dir = cursor_dir / "rules"
    rules_dir.mkdir(parents=True, exist_ok=True)
    rule_file = rules_dir / "memory.mdc"
    rule_file.write_text(_CURSOR_RULE)
    actions.append(f"  ✅ {rule_file.relative_to(project_dir)}")

    return actions


def _write_claude(project_dir: Path, mode: str, db_url: str | None = None, **embed_opts: str) -> list[str]:
    actions = []

    # MCP config for Claude Code
    claude_dir = project_dir / ".claude"
    claude_dir.mkdir(parents=True, exist_ok=True)
    mcp_file = claude_dir / "mcp.json"

    if mcp_file.exists():
        config = json.loads(mcp_file.read_text())
    else:
        config = {"mcpServers": {}}

    config.setdefault("mcpServers", {})
    config["mcpServers"]["mo-memory"] = _mcp_config(mode, db_url, **embed_opts)
    mcp_file.write_text(json.dumps(config, indent=2) + "\n")
    actions.append(f"  ✅ {mcp_file.relative_to(project_dir)}")

    # Append to CLAUDE.md
    claude_md = project_dir / "CLAUDE.md"
    if claude_md.exists():
        existing = claude_md.read_text()
        if "mo-memory" not in existing:
            claude_md.write_text(existing.rstrip() + "\n" + _CLAUDE_RULE)
            actions.append(f"  ✅ {claude_md.relative_to(project_dir)} (appended)")
        else:
            actions.append(f"  ⏭️  {claude_md.relative_to(project_dir)} (already configured)")
    else:
        claude_md.write_text(_CLAUDE_RULE.lstrip())
        actions.append(f"  ✅ {claude_md.relative_to(project_dir)} (created)")

    return actions


# ── Commands ──────────────────────────────────────────────────────────

def cmd_init(args: argparse.Namespace) -> None:
    project_dir = Path(args.dir).resolve()
    mode = args.mode
    db_url = args.db_url
    embed_opts = {}
    for key in ("provider", "model", "dim", "api_key", "base_url"):
        val = getattr(args, f"embedding_{key}", None)
        if val:
            embed_opts[key] = str(val)

    tools = _detect_tools(project_dir)

    detected = [name for name, found in tools.items() if found]
    if not detected:
        print("No AI tools detected. Creating configs for all tools.")
        detected = ["kiro", "cursor", "claude"]

    print(f"Detected tools: {', '.join(detected)}")
    print(f"Mode: {mode}")
    if db_url:
        print(f"DB URL: {db_url}")
    if embed_opts.get("provider"):
        print(f"Embedding: {embed_opts['provider']}")
    print()

    writers = {"kiro": _write_kiro, "cursor": _write_cursor, "claude": _write_claude}
    for tool_name in detected:
        print(f"Configuring {tool_name}:")
        actions = writers[tool_name](project_dir, mode, db_url, **embed_opts)
        for a in actions:
            print(a)
        print()

    print("Done! Restart your AI tools to pick up the new MCP config.")
    if mode == "stdio" and not db_url:
        print("\nTip: pass --db-url to connect to a specific database:")
        print("  mo-memory init --db-url 'mysql+pymysql://user:pass@host:6001/db'")


def cmd_status(args: argparse.Namespace) -> None:
    project_dir = Path(args.dir).resolve()
    tools = _detect_tools(project_dir)

    for name, found in tools.items():
        if found:
            # Check if MCP config exists
            if name == "kiro":
                cfg = project_dir / ".kiro" / "settings" / "mcp.json"
            elif name == "cursor":
                cfg = project_dir / ".cursor" / "mcp.json"
            else:
                cfg = project_dir / ".claude" / "mcp.json"

            if cfg.exists():
                data = json.loads(cfg.read_text())
                has_memory = "mo-memory" in data.get("mcpServers", {})
                print(f"  {name}: {'✅ configured' if has_memory else '❌ not configured'}")
            else:
                print(f"  {name}: ❌ no MCP config")
        else:
            print(f"  {name}: — not detected")


def cmd_health(args: argparse.Namespace) -> None:
    import urllib.request
    url = args.api_url.rstrip("/") + "/health"
    try:
        with urllib.request.urlopen(url, timeout=5) as resp:
            data = json.loads(resp.read())
            status = data.get("status", "unknown")
            print(f"Memory service: {status}")
            print(f"Database: {data.get('database', 'unknown')}")
    except Exception as e:
        print(f"❌ Cannot reach memory service at {url}: {e}")


def cmd_migrate(args: argparse.Namespace) -> None:
    """Create memory tables in the database."""
    db_url = args.db_url or os.environ.get("MO_MEMORY_DB_URL")

    if db_url:
        from sqlalchemy import create_engine
        engine = create_engine(db_url, pool_pre_ping=True)
    else:
        try:
            from api.database import engine
        except Exception:
            print("❌ --db-url required (or set MO_MEMORY_DB_URL, or run from project root)")
            sys.exit(1)

    from api.base import Base
    import core.memory.models  # noqa: F401

    memory_tables = [
        t for t in Base.metadata.sorted_tables
        if t.name.startswith("mem_") or t.name.startswith("memory_graph")
    ]

    Base.metadata.create_all(bind=engine, tables=memory_tables, checkfirst=True)

    for t in memory_tables:
        print(f"  ✅ {t.name}")

    print(f"\n{len(memory_tables)} memory tables ready.")


def main() -> None:
    parser = argparse.ArgumentParser(prog="mo-memory", description="Configure AI tools for shared memory")
    parser.add_argument("--dir", default=".", help="Project directory")
    sub = parser.add_subparsers(dest="command")

    p_init = sub.add_parser("init", help="Configure MCP + steering rules")
    p_init.add_argument("--mode", choices=["stdio", "remote"], default="stdio", help="MCP transport mode")
    p_init.add_argument("--db-url", help="Database URL, e.g. mysql+pymysql://user:pass@host:6001/db")
    p_init.add_argument("--embedding-provider", help="Embedding provider: local (default), openai, mock")
    p_init.add_argument("--embedding-model", help="Embedding model name (default: all-MiniLM-L6-v2)")
    p_init.add_argument("--embedding-dim", help="Embedding dimension (default: 384)")
    p_init.add_argument("--embedding-api-key", help="API key for OpenAI embedding provider")
    p_init.add_argument("--embedding-base-url", help="Custom API base URL (e.g. Ollama)")

    sub.add_parser("status", help="Show configuration status")

    p_migrate = sub.add_parser("migrate", help="Create memory tables in the database")
    p_migrate.add_argument("--db-url", help="Database URL (or set MO_MEMORY_DB_URL)")

    p_health = sub.add_parser("health", help="Check memory service health")
    p_health.add_argument("--api-url", default="http://localhost:8100", help="Memory service URL")

    args = parser.parse_args()
    if args.command == "init":
        cmd_init(args)
    elif args.command == "status":
        cmd_status(args)
    elif args.command == "migrate":
        cmd_migrate(args)
    elif args.command == "health":
        cmd_health(args)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
