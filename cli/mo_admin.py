#!/usr/bin/env python3
"""mo-admin CLI - Administrative interface for mo-agent-engine."""

import json
import sys
from datetime import datetime
from pathlib import Path

import click

# Add project root to path
sys.path.insert(0, str(Path(__file__).parent.parent))

from api.database import get_db_session
from core.auth.audit_logger import AuditLogger
from core.auth.permission_checker import PermissionChecker
from core.llm.models import LLMProvider


@click.group()
@click.version_option(version="0.1.0")
@click.option("--user", envvar="MO_ADMIN_USER", help="Admin user (or set MO_ADMIN_USER env var)")
@click.pass_context
def cli(ctx, user):
    """mo-admin - Administrative CLI for mo-agent-engine."""
    ctx.ensure_object(dict)
    ctx.obj["user"] = user or "admin"
    db = next(get_db_session())
    ctx.obj["db"] = db
    ctx.obj["checker"] = PermissionChecker(db)
    ctx.obj["audit"] = AuditLogger(db)


@cli.group()
def model():
    """Manage LLM models."""
    pass


@model.command("add")
@click.argument("model_name")
@click.argument("provider", type=click.Choice([p.value for p in LLMProvider]))
@click.option("--scope", type=click.Choice(["global", "account", "user"]), required=True)
@click.option("--scope-id", help="Account or user ID for scope")
@click.option("--context-window", default=128000, help="Context window size")
@click.option("--price-prompt", default=0.01, help="Price per 1K prompt tokens")
@click.option("--price-completion", default=0.03, help="Price per 1K completion tokens")
@click.option("--rpm-limit", default=500, help="Requests per minute limit")
@click.option("--tpm-limit", default=150000, help="Tokens per minute limit")
@click.option("--fallback", help="Fallback model name")
@click.option("--tags", help="Comma-separated tags")
@click.pass_context
def model_add(
    ctx,
    model_name,
    provider,
    scope,
    scope_id,
    context_window,
    price_prompt,
    price_completion,
    rpm_limit,
    tpm_limit,
    fallback,
    tags,
):
    """Add a new model to the registry."""
    db = ctx.obj["db"]
    checker = ctx.obj["checker"]
    audit = ctx.obj["audit"]
    user = ctx.obj["user"]

    # Permission check
    if not checker.can_manage_models(user, scope, scope_id):
        click.echo(f"❌ Permission denied: {user} cannot manage {scope} models")
        sys.exit(1)

    # Validate scope_id
    if scope in ["account", "user"] and not scope_id:
        click.echo(f"❌ --scope-id is required for {scope} scope")
        sys.exit(1)

    try:
        # Generate config_id
        config_id = f"model_{scope}_{scope_id or 'global'}_{model_name}"

        # Parse tags
        tag_list = [t.strip() for t in tags.split(",")] if tags else []

        # Create model config JSON
        config = {
            "model_name": model_name,
            "provider": provider,
            "context_window": context_window,
            "price_per_1k_prompt": price_prompt,
            "price_per_1k_completion": price_completion,
            "rpm_limit": rpm_limit,
            "tpm_limit": tpm_limit,
            "fallback_to": fallback,
            "tags": tag_list,
            "is_active": True,
        }

        # Insert using ORM
        from api.models import Config
        cfg = Config(
            config_id=config_id,
            key_name=model_name,
            value=json.dumps(config),
            description=f"Model config for {model_name}",
        )
        db.add(cfg)
        db.commit()

        # Audit log
        audit.log_model_add(user, model_name, scope, scope_id)

        click.echo(f"✅ Model '{model_name}' added successfully")
        click.echo(f"   Provider: {provider}")
        click.echo(f"   Scope: {scope}" + (f" ({scope_id})" if scope_id else ""))
        click.echo(f"   Config ID: {config_id}")
    except Exception as e:
        click.echo(f"❌ Failed to add model: {e}")
        sys.exit(1)


@model.command("remove")
@click.argument("model_name")
@click.option("--scope", type=click.Choice(["global", "account", "user"]), required=True)
@click.option("--scope-id", help="Account or user ID for scope")
@click.option("--force", is_flag=True, help="Force removal without confirmation")
@click.pass_context
def model_remove(ctx, model_name, scope, scope_id, force):
    """Remove a model from the registry."""
    db = ctx.obj["db"]
    checker = ctx.obj["checker"]
    audit = ctx.obj["audit"]
    user = ctx.obj["user"]

    # Permission check
    if not checker.can_manage_models(user, scope, scope_id):
        click.echo(f"❌ Permission denied: {user} cannot manage {scope} models")
        sys.exit(1)

    # Validate scope_id
    if scope in ["account", "user"] and not scope_id:
        click.echo(f"❌ --scope-id is required for {scope} scope")
        sys.exit(1)

    try:
        # Check if model exists
        from sqlalchemy import text
        
        result = db.execute(
            text("SELECT * FROM configs WHERE key_name = :model_name"),
            {"model_name": model_name}
        )
        row = result.first()

        if not row:
            click.echo(f"❌ Model '{model_name}' not found")
            sys.exit(1)

        # Confirm removal
        if not force:
            if not click.confirm(f"Remove model '{model_name}'?"):
                click.echo("Cancelled")
                return

        # Delete
        db.execute(
            text("DELETE FROM configs WHERE config_id = :config_id"),
            {"config_id": row._mapping["config_id"]}
        )
        db.commit()

        # Audit log
        audit.log_model_remove(user, model_name, scope, scope_id)

        click.echo(f"✅ Model '{model_name}' removed successfully")
    except Exception as e:
        click.echo(f"❌ Failed to remove model: {e}")
        sys.exit(1)


@model.command("list")
@click.option("--scope", type=click.Choice(["global", "account", "user"]), help="Filter by scope")
@click.option("--scope-id", help="Filter by scope ID")
@click.pass_context
def model_list(ctx, scope, scope_id):
    """List all models."""
    from sqlalchemy import text
    
    db = ctx.obj["db"]

    result = db.execute(text("SELECT * FROM configs WHERE key_name LIKE '%gpt%' OR key_name LIKE 'model_%'"))
    
    models = [dict(row._mapping) for row in result]

    if not models:
        click.echo("No models found")
        return

    click.echo("Models:")
    click.echo("=" * 80)

    for m in models:
        config = json.loads(m["value"])
        click.echo(f"✓ {m['key_name']}")
        click.echo(f"   Provider: {config.get('provider', 'N/A')}")
        click.echo(f"   Context Window: {config.get('context_window', 'N/A')}")
        click.echo()



@cli.group()
def token():
    """Manage API tokens."""
    pass


@token.command("create")
@click.option("--type", "token_type", type=click.Choice(["llm", "github"]), required=True)
@click.option("--provider", required=True, help="Provider name (e.g., openai, deepseek, anthropic)")
@click.option("--scope", type=click.Choice(["global", "user"]), required=True)
@click.option("--scope-id", help="User ID for user scope")
@click.option("--base-url", help="API base URL (for OpenAI-compatible providers, e.g. https://api.deepseek.com/v1)")
@click.option("--token-value", prompt="API Key", hide_input=True, help="API key")
@click.pass_context
def token_create(ctx, token_type, provider, scope, scope_id, base_url, token_value):
    """Create a new API token."""
    db = ctx.obj["db"]
    checker = ctx.obj["checker"]
    audit = ctx.obj["audit"]
    user = ctx.obj["user"]

    if not checker.is_admin(user):
        click.echo("❌ Permission denied: only admins can create tokens")
        sys.exit(1)

    if scope == "user" and not scope_id:
        click.echo("❌ --scope-id is required for user scope")
        sys.exit(1)

    try:
        from uuid_utils import uuid7
        token_id = str(uuid7())
        scope_user_id = scope_id if scope == "user" else None
        metadata = json.dumps({"base_url": base_url}) if base_url else None

        from sqlalchemy import text
        db.execute(
            text("""
            INSERT INTO tokens
            (token_id, type, provider, scope_user_id,
             encrypted_value, is_active, metadata, created_at)
            VALUES (:token_id, :token_type, :provider, :scope_user_id,
                    :token_value, TRUE, :metadata, :created_at)
            """),
            {
                "token_id": token_id,
                "token_type": token_type,
                "provider": provider,
                "scope_user_id": scope_user_id,
                "token_value": token_value,
                "metadata": metadata,
                "created_at": datetime.now(),
            }
        )
        db.commit()

        audit.log_token_create(user, token_type, provider, scope)

        click.echo("✅ Token created successfully")
        click.echo(f"   Token ID: {token_id}")
        click.echo(f"   Provider: {provider}")
        if base_url:
            click.echo(f"   Base URL: {base_url}")
        click.echo(f"   Scope: {scope}" + (f" ({scope_id})" if scope_id else ""))
    except Exception as e:
        click.echo(f"❌ Failed to create token: {e}")
        sys.exit(1)


@token.command("list")
@click.option("--scope", type=click.Choice(["global", "user"]))
@click.option("--scope-id")
@click.pass_context
def token_list(ctx, scope, scope_id):
    """List API tokens (values hidden)."""
    from sqlalchemy import text
    
    db = ctx.obj["db"]

    query = """
        SELECT token_id, type, provider, scope_user_id,
               is_active, created_at 
        FROM tokens
        WHERE 1=1
    """
    params = {}

    if scope == "user" and scope_id:
        query += " AND scope_user_id = :scope_id"
        params["scope_id"] = scope_id
    elif scope == "global":
        query += " AND scope_user_id IS NULL"

    query += " ORDER BY created_at DESC"

    result = db.execute(text(query), params)
    tokens = [dict(row._mapping) for row in result]

    if not tokens:
        click.echo("No tokens found")
        return

    click.echo("API Tokens:")
    click.echo("=" * 80)

    for t in tokens:
        status = "✓" if t["is_active"] else "✗"
        click.echo(f"{status} {t['token_id']}")
        click.echo(f"   Type: {t['type']}, Provider: {t['provider']}")
        
        # Determine scope
        if t["scope_user_id"]:
            scope_str = f"user ({t['scope_user_id']})"
        else:
            scope_str = "global"
        
        click.echo(f"   Scope: {scope_str}")
        click.echo(f"   Created: {t['created_at']}")
        click.echo()


@cli.group()
def audit():
    """View audit logs."""
    pass


@audit.command("logs")
@click.option("--user-id", help="Filter by user")
@click.option("--action", help="Filter by action")
@click.option("--resource-type", help="Filter by resource type")
@click.option("--since", help="Filter by date (YYYY-MM-DD)")
@click.option("--limit", default=50, help="Number of logs to show")
@click.pass_context
def audit_logs(ctx, user_id, action, resource_type, since, limit):
    """View audit logs."""
    db = ctx.obj["db"]
    checker = ctx.obj["checker"]
    user = ctx.obj["user"]

    # Permission check
    if not checker.can_view_audit_logs(user, user_id):
        click.echo("❌ Permission denied: cannot view audit logs")
        sys.exit(1)

    audit_logger = AuditLogger(db)

    since_dt = datetime.fromisoformat(since) if since else None
    logs = audit_logger.get_logs(
        user_id=user_id, action=action, resource_type=resource_type, since=since_dt, limit=limit
    )

    if not logs:
        click.echo("No audit logs found")
        return

    click.echo("Audit Logs:")
    click.echo("=" * 80)

    for log in logs:
        status_icon = "✓" if log["status"] == "success" else "✗"
        click.echo(f"{status_icon} [{log['created_at']}] {log['user_id']}")
        click.echo(f"   Action: {log['action']}")
        click.echo(f"   Resource: {log['resource_type']} / {log['resource_id']}")
        if log["details"]:
            click.echo(f"   Details: {log['details']}")
        click.echo()


@cli.group()
def prompt():
    """Manage prompt templates."""
    pass


@prompt.command("optimize")
@click.option("--template", default="system_general", help="Template ID to optimize")
@click.option("--min-cases", default=3, help="Minimum low-score cases needed")
@click.option("--dry-run", is_flag=True, help="Generate improvement without activating")
def prompt_optimize(template, min_cases, dry_run):
    """Auto-optimize a prompt template based on feedback."""
    from api.database import get_db_session
    from core.context.prompt_optimizer import PromptOptimizer
    from core.llm.client import LLMClient

    db = next(get_db_session())
    llm = LLMClient(db)
    optimizer = PromptOptimizer(db, llm)

    click.echo(f"🔄 Optimizing prompt: {template} (dry_run={dry_run})")
    result = optimizer.optimize(template, min_cases=min_cases, dry_run=dry_run)

    if result.error:
        click.echo(f"⚠️  {result.error}")
        return

    click.echo(f"📊 Cases analyzed: {result.cases_analyzed}")
    click.echo(f"🔍 Diagnosis: {result.diagnosis}")
    click.echo(f"🔒 Gate verdict: {result.gate_verdict}")

    if result.new_content:
        click.echo(f"\n📝 New prompt ({result.new_version}):")
        click.echo("-" * 40)
        click.echo(result.new_content[:500])
        if len(result.new_content) > 500:
            click.echo("...")
        click.echo("-" * 40)

    if result.activated:
        click.echo(f"✅ Activated: {template} v{result.new_version}")
    elif dry_run and result.new_content:
        click.echo("ℹ️  Dry run — not activated. Remove --dry-run to apply.")


@prompt.command("list")
def prompt_list():
    """List all prompt templates."""
    from api.database import get_db_session
    from sqlalchemy import text as sql_text

    db = next(get_db_session())
    rows = db.execute(sql_text(
        "SELECT template_id, version, is_active, LEFT(content, 60) as preview, created_at "
        "FROM prompt_templates ORDER BY template_id, created_at DESC"
    )).fetchall()

    if not rows:
        click.echo("No prompt templates found")
        return

    click.echo(f"{'Template':<25} {'Version':<8} {'Active':<7} {'Preview'}")
    click.echo("=" * 80)
    for r in rows:
        active = "✓" if r[2] else " "
        click.echo(f"{r[0]:<25} {r[1]:<8} {active:<7} {r[3]}")


@cli.command()
@click.option("--reset", is_flag=True, help="Drop and recreate database")
def init(reset):
    """Initialize agent system: database, tables, admin user, RBAC."""
    import pymysql
    from config.settings import get_settings

    settings = get_settings()
    db_name = settings.matrixone_database

    click.echo("🔧 Initializing agent system...")

    # 1. Connect without database to create it
    conn = pymysql.connect(
        host=settings.matrixone_host,
        port=settings.matrixone_port,
        user=settings.matrixone_user,
        password=settings.matrixone_password,
        autocommit=True,
    )
    cursor = conn.cursor()

    if reset:
        if click.confirm(f"⚠️  Drop database '{db_name}' and recreate?", abort=True):
            cursor.execute(f"DROP DATABASE IF EXISTS {db_name}")
            click.echo(f"   Dropped {db_name}")

    cursor.execute(f"CREATE DATABASE IF NOT EXISTS {db_name}")
    click.echo(f"✅ Database '{db_name}' ready")
    cursor.close()
    conn.close()

    # 2. Create tables via MatrixOne dialect (supports vecf32)
    from matrixone import Client as MoClient
    from api.models import Base
    from sqlalchemy import text
    from sqlalchemy.schema import CreateTable

    client = MoClient(
        host=settings.matrixone_host,
        port=settings.matrixone_port,
        user=settings.matrixone_user,
        password=settings.matrixone_password,
        database=db_name,
        sql_log_mode="off",
    )
    eng = client._engine

    with eng.connect() as c:
        existing = {row[0] for row in c.execute(text("SHOW TABLES")).fetchall()}
        created = 0
        for table in Base.metadata.sorted_tables:
            if table.name in existing:
                continue
            try:
                ddl = str(CreateTable(table).compile(dialect=eng.dialect))
                c.execute(text(ddl))
                c.execute(text("COMMIT"))
                created += 1
            except Exception as e:
                click.echo(f"   ⚠️  {table.name}: {e}", err=True)

        click.echo(f"✅ Tables ready ({created} created, {len(existing)} existed)")

        # 3. Fulltext index
        try:
            c.execute(text("CREATE FULLTEXT INDEX ft_content_session ON conversation_events (content, session_id) WITH PARSER ngram"))
            c.execute(text("COMMIT"))
            click.echo("✅ Fulltext index created")
        except Exception:
            pass  # already exists

        # 4. Admin user + role
        c.execute(text("INSERT IGNORE INTO users (user_id, username, email, password_hash, is_active, created_at) VALUES ('admin', 'admin', 'admin@local', 'x', 1, NOW())"))
        c.execute(text("INSERT IGNORE INTO roles (role_id, role_name, description, created_at) VALUES ('role_admin', 'mo_agent_admin', 'System admin', NOW())"))
        c.execute(text("INSERT IGNORE INTO user_roles (user_id, role_id, created_at) VALUES ('admin', 'role_admin', NOW())"))
        c.execute(text("COMMIT"))
        click.echo("✅ Admin user ready")

        # 5. Default prompt templates
        _prompts = [
            ("system_general", (
                "You are an intelligent development agent.\n\n"
                "Capabilities:\n"
                "- Write, review, and refactor code across languages\n"
                "- Analyze errors, logs, and stack traces\n"
                "- Plan tasks and design architectures\n"
                "- Execute tools and coordinate with other agents\n\n"
                "When using tools, explain what you're doing and why.\n"
                "When you don't know something, say so."
            )),
            ("system_code_review", (
                "You are an expert code reviewer.\n\n"
                "Focus areas:\n"
                "- Correctness: logic errors, edge cases, off-by-one\n"
                "- Security: injection, auth bypass, data exposure\n"
                "- Performance: unnecessary allocations, N+1 queries, blocking I/O\n"
                "- Maintainability: naming, complexity, SOLID principles\n\n"
                "For each issue found, provide: severity, location, explanation, and fix."
            )),
            ("system_planning", (
                "You are a technical architect.\n\n"
                "Approach:\n"
                "1. Clarify requirements and constraints\n"
                "2. Identify key decisions and trade-offs\n"
                "3. Propose solution with components and interfaces\n"
                "4. Call out risks and unknowns\n\n"
                "Think step-by-step. Prefer simple solutions over clever ones."
            )),
            ("system_debugging", (
                "You are a debugging expert.\n\n"
                "Approach:\n"
                "1. Reproduce: understand the exact failure condition\n"
                "2. Isolate: narrow down to the smallest failing unit\n"
                "3. Root cause: explain WHY it fails, not just WHERE\n"
                "4. Fix: provide a minimal, targeted fix\n"
                "5. Verify: suggest how to confirm the fix works\n\n"
                "Always show your reasoning chain."
            )),
        ]
        for tid, content in _prompts:
            c.execute(text(
                "INSERT IGNORE INTO prompt_templates (template_id, version, content, is_active, created_at, updated_at) "
                "VALUES (:tid, '1.0', :content, 1, NOW(), NOW())"
            ), {"tid": tid, "content": content})
        c.execute(text("COMMIT"))
        click.echo("✅ Prompt templates ready")

    # 5. Agent config (existing script)
    import subprocess
    result = subprocess.run(["make", "db-init-agent"], capture_output=True, text=True)
    if result.returncode == 0:
        click.echo("✅ Agent config ready")
    else:
        click.echo(f"   ⚠️  Agent config: {result.stderr.strip()}", err=True)

    click.echo("\n🎉 System initialized. Next: mo-admin token create --type llm --provider openai --scope global --token-value sk-...")


if __name__ == "__main__":
    cli()
