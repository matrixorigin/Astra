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
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from core.db_consumer import DbFactory

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


# ── Steering rule content (loaded from templates/) ───────────────────
# Templates live alongside the MCP server package so they are the single
# source of truth for all AI-tool steering rules.  The CLI reads them at
# runtime and writes them into each tool's config directory.

_TEMPLATES_DIR = Path(__file__).parent.parent / "mo_memory_mcp" / "templates"

# Required sections that every steering template must contain.
# Prevents silently writing empty or broken rules.
_REQUIRED_KEYWORDS = ["Memory Integration", "memory_retrieve"]


def _load_template(name: str) -> str:
    """Load and validate a steering-rule template.

    Raises:
        FileNotFoundError: Template file does not exist.
        ValueError: Template is empty or missing required sections.
    """
    path = _TEMPLATES_DIR / name
    if not path.exists():
        raise FileNotFoundError(f"Template not found: {path}")

    content = path.read_text()
    if not content.strip():
        raise ValueError(f"Template is empty: {path}")

    for keyword in _REQUIRED_KEYWORDS:
        if keyword not in content:
            raise ValueError(
                f"Template {path.name} missing required section '{keyword}'"
            )
    return content


def _get_kiro_steering() -> str:
    return _load_template("kiro_steering.md")


def _get_cursor_rule() -> str:
    return _load_template("cursor_rule.md")


def _get_claude_rule() -> str:
    return _load_template("claude_rule.md")


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
    rule_file.write_text(_get_kiro_steering())
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
    rule_file.write_text(_get_cursor_rule())
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
            claude_md.write_text(existing.rstrip() + "\n" + _get_claude_rule())
            actions.append(f"  ✅ {claude_md.relative_to(project_dir)} (appended)")
        else:
            actions.append(f"  ⏭️  {claude_md.relative_to(project_dir)} (already configured)")
    else:
        claude_md.write_text(_get_claude_rule().lstrip())
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

    # Auto-migrate: create memory tables if not exist.
    # This is best-effort — if it fails, the user can run 'mo-memory migrate'
    # manually.  We warn loudly so the failure is not missed.
    try:
        if db_url:
            from sqlalchemy import create_engine
            _engine = create_engine(db_url, pool_pre_ping=True)
        else:
            from api.database import engine as _engine
        import core.memory.models  # noqa: F401
        from api.base import Base
        memory_tables = [t for t in Base.metadata.sorted_tables
                         if t.name.startswith("mem_") or t.name.startswith("memory_graph")]
        Base.metadata.create_all(bind=_engine, tables=memory_tables, checkfirst=True)
        print(f"✅ Memory tables ready ({len(memory_tables)} tables)")
    except ImportError as e:
        print(f"⚠️  Could not auto-migrate tables (missing dependency): {e}")
        print("   Run 'mo-memory migrate' manually after setup.")
    except Exception as e:
        print(f"⚠️  Could not auto-migrate tables: {e}")
        print("   Run 'mo-memory migrate' manually after setup.")
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

    import core.memory.models  # noqa: F401
    from api.base import Base

    memory_tables = [
        t for t in Base.metadata.sorted_tables
        if t.name.startswith("mem_") or t.name.startswith("memory_graph")
    ]

    Base.metadata.create_all(bind=engine, tables=memory_tables, checkfirst=True)

    for t in memory_tables:
        print(f"  ✅ {t.name}")

    print(f"\n{len(memory_tables)} memory tables ready.")


def _get_db_factory(args: argparse.Namespace) -> DbFactory:
    """Resolve a DbFactory from CLI args, env var, or project default.

    Resolution order:
      1. --db-url argument
      2. MO_MEMORY_DB_URL environment variable
      3. api.database.SessionLocal (project default)

    The returned factory is a short-lived CLI tool — engine disposal
    happens at process exit.
    """
    db_url = getattr(args, "db_url", None) or os.environ.get("MO_MEMORY_DB_URL")
    if db_url:
        from sqlalchemy import create_engine
        from sqlalchemy.orm import sessionmaker
        engine = create_engine(db_url, pool_pre_ping=True)
        return sessionmaker(bind=engine)
    try:
        from api.database import SessionLocal
        return SessionLocal
    except ImportError:
        print("❌ Database not available.")
        print("   Use --db-url, set MO_MEMORY_DB_URL, or run from project root.")
        sys.exit(1)


def cmd_governance(args: argparse.Namespace) -> None:
    """Run memory governance cycle."""
    from core.memory.tabular.governance import GovernanceScheduler
    db_factory = _get_db_factory(args)
    gs = GovernanceScheduler(db_factory)
    user_id = args.user_id or "all"
    print(f"Running governance for user={user_id}...")
    result = gs.run_cycle(user_id)
    print(f"  quarantined={result.quarantined}")
    print(f"  cleaned_stale={result.cleaned_stale}")
    print(f"  scenes_created={result.scenes_created}")
    for table, h in result.vector_index_health.items():
        if h.get("rebuilt"):
            print(f"  ✅ {table}: IVF index rebuilt")
        elif h.get("needs_rebuild"):
            print(f"  ⚠️  {table}: IVF index needs rebuild (ratio={h.get('ratio')})")
        elif "error" not in h:
            print(f"  ✅ {table}: IVF index healthy (ratio={h.get('ratio')})")
    if result.errors:
        for e in result.errors:
            print(f"  ❌ {e}")


def cmd_consolidate(args: argparse.Namespace) -> None:
    """Run graph consolidation."""
    from core.memory.graph.consolidation import GraphConsolidator
    db_factory = _get_db_factory(args)
    gc = GraphConsolidator(db_factory)
    print(f"Running consolidation for user={args.user_id}...")
    result = gc.consolidate(args.user_id)
    print(f"  merged_nodes={result.merged_nodes}")
    print(f"  conflicts_detected={result.conflicts_detected}")
    print(f"  orphaned_scenes={result.orphaned_scenes}")
    print(f"  promoted={result.promoted}, demoted={result.demoted}")
    if result.errors:
        for e in result.errors:
            print(f"  ❌ {e}")


def cmd_reflect(args: argparse.Namespace) -> None:
    """Run reflection (requires LLM)."""
    from core.memory.graph.candidates import GraphCandidateProvider
    from core.memory.graph.service import GraphMemoryService
    from core.memory.reflection.engine import ReflectionEngine
    db_factory = _get_db_factory(args)
    try:
        from core.llm.client import LLMClient
        llm = LLMClient(db_factory=db_factory)
    except ImportError as e:
        print(f"❌ LLM client not available (missing dependency): {e}")
        sys.exit(1)
    except Exception as e:
        print(f"❌ LLM client initialization failed: {e}")
        sys.exit(1)
    provider = GraphCandidateProvider(db_factory)
    svc = GraphMemoryService(db_factory)
    engine = ReflectionEngine(provider, svc, llm)
    print(f"Running reflection for user={args.user_id}...")
    result = engine.reflect(args.user_id)
    print(f"  candidates_found={result.candidates_found}")
    print(f"  scenes_created={result.scenes_created}")
    print(f"  llm_calls={result.llm_calls}")
    if result.errors:
        for e in result.errors:
            print(f"  ❌ {e}")


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

    p_gov = sub.add_parser("governance", help="Run memory governance: quarantine, cleanup, IVF index health")
    p_gov.add_argument("--user-id", help="User ID (default: all users)")
    p_gov.add_argument("--db-url", help="Database URL (or set MO_MEMORY_DB_URL)")

    p_con = sub.add_parser("consolidate", help="Run graph consolidation: conflict detection, orphan cleanup")
    p_con.add_argument("--user-id", required=True, help="User ID")
    p_con.add_argument("--db-url", help="Database URL (or set MO_MEMORY_DB_URL)")

    p_ref = sub.add_parser("reflect", help="Run reflection: synthesize insights from memory clusters (requires LLM)")
    p_ref.add_argument("--user-id", required=True, help="User ID")
    p_ref.add_argument("--db-url", help="Database URL (or set MO_MEMORY_DB_URL)")

    args = parser.parse_args()
    if args.command == "init":
        cmd_init(args)
    elif args.command == "status":
        cmd_status(args)
    elif args.command == "migrate":
        cmd_migrate(args)
    elif args.command == "health":
        cmd_health(args)
    elif args.command == "governance":
        cmd_governance(args)
    elif args.command == "consolidate":
        cmd_consolidate(args)
    elif args.command == "reflect":
        cmd_reflect(args)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
