#!/usr/bin/env python3
"""mo-admin CLI - Administrative interface for mo-agent-engine."""

import json
import sys
from datetime import datetime
from pathlib import Path

import click

# Add project root to path
sys.path.insert(0, str(Path(__file__).parent.parent))

from core.auth.audit_logger import AuditLogger
from core.auth.permission_checker import PermissionChecker
from core.llm.models import LLMProvider
from sdk import Database


@click.group()
@click.version_option(version="0.1.0")
@click.option("--user", envvar="MO_ADMIN_USER", help="Admin user (or set MO_ADMIN_USER env var)")
@click.pass_context
def cli(ctx, user):
    """mo-admin - Administrative CLI for mo-agent-engine."""
    ctx.ensure_object(dict)
    ctx.obj["user"] = user or "admin"
    ctx.obj["db"] = Database()
    ctx.obj["checker"] = PermissionChecker(ctx.obj["db"])
    ctx.obj["audit"] = AuditLogger(ctx.obj["db"])


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

        # Insert into model_registry
        db.execute(
            """
            INSERT INTO agent_config.model_registry
            (config_id, scope_type, scope_id, model_name, provider, config_json, created_by, created_at)
            VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
            """,
            (
                config_id,
                scope,
                scope_id,
                model_name,
                provider,
                json.dumps(config),
                user,
                datetime.now(),
            ),
        )

        # Audit log
        audit.log_model_add(user, model_name, scope, scope_id)

        click.echo(f"✅ Model '{model_name}' added successfully")
        click.echo(f"   Provider: {provider}")
        click.echo(f"   Scope: {scope}" + (f" ({scope_id})" if scope_id else ""))
        click.echo(f"   Config ID: {config_id}")
    except Exception as e:
        audit.log(
            user, "add_model", "model", model_name, status="failed", details={"error": str(e)}
        )
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
        query = """
        SELECT * FROM agent_config.model_registry
        WHERE model_name = %s AND scope_type = %s
        """
        params = [model_name, scope]

        if scope_id:
            query += " AND scope_id = %s"
            params.append(scope_id)
        else:
            query += " AND scope_id IS NULL"

        row = db.fetchone(query, tuple(params))

        if not row:
            click.echo(f"❌ Model '{model_name}' not found in {scope} scope")
            sys.exit(1)

        # Confirm removal
        if not force:
            if not click.confirm(f"Remove model '{model_name}' from {scope} scope?"):
                click.echo("Cancelled")
                return

        # Delete
        db.execute(
            "DELETE FROM agent_config.model_registry WHERE config_id = %s", (row["config_id"],)
        )

        # Audit log
        audit.log_model_remove(user, model_name, scope, scope_id)

        click.echo(f"✅ Model '{model_name}' removed successfully")
    except Exception as e:
        audit.log(
            user, "remove_model", "model", model_name, status="failed", details={"error": str(e)}
        )
        click.echo(f"❌ Failed to remove model: {e}")
        sys.exit(1)


@model.command("list")
@click.option("--scope", type=click.Choice(["global", "account", "user"]), help="Filter by scope")
@click.option("--scope-id", help="Filter by scope ID")
@click.pass_context
def model_list(ctx, scope, scope_id):
    """List all models."""
    db = ctx.obj["db"]

    query = "SELECT * FROM agent_config.model_registry"
    conditions = []
    params = []

    if scope:
        conditions.append("scope_type = %s")
        params.append(scope)

    if scope_id:
        conditions.append("scope_id = %s")
        params.append(scope_id)

    if conditions:
        query += " WHERE " + " AND ".join(conditions)

    query += " ORDER BY scope_type, model_name"

    models = db.fetchall(query, tuple(params))

    if not models:
        click.echo("No models found")
        return

    click.echo("Models:")
    click.echo("=" * 80)

    for m in models:
        config = json.loads(m["config_json"])
        click.echo(f"✓ {m['model_name']}")
        click.echo(f"   Provider: {m['provider']}")
        click.echo(
            f"   Scope: {m['scope_type']}" + (f" ({m['scope_id']})" if m["scope_id"] else "")
        )
        click.echo(
            f"   Price: ${config['price_per_1k_prompt']:.4f}/1K prompt + ${config['price_per_1k_completion']:.4f}/1K completion"
        )
        click.echo()


@cli.group()
def token():
    """Manage API tokens."""
    pass


@token.command("create")
@click.option("--type", "token_type", type=click.Choice(["llm", "github"]), required=True)
@click.option("--provider", required=True, help="Provider name (e.g., openai, anthropic, groq)")
@click.option("--scope", type=click.Choice(["global", "account", "user"]), required=True)
@click.option("--scope-id", help="Account or user ID for scope")
@click.option("--token-value", prompt=True, hide_input=True, help="API token value")
@click.pass_context
def token_create(ctx, token_type, provider, scope, scope_id, token_value):
    """Create a new API token."""
    db = ctx.obj["db"]
    checker = ctx.obj["checker"]
    audit = ctx.obj["audit"]
    user = ctx.obj["user"]

    # Permission check
    if not checker.is_admin(user):
        click.echo("❌ Permission denied: only admins can create tokens")
        sys.exit(1)

    # Validate scope_id
    if scope in ["account", "user"] and not scope_id:
        click.echo(f"❌ --scope-id is required for {scope} scope")
        sys.exit(1)

    try:
        from uuid_utils import uuid7
        token_id = str(uuid7())

        # Map scope to tokens table columns
        scope_user_id = scope_id if scope == "user" else None
        scope_tenant_id = scope_id if scope == "account" else None

        # Insert into tokens table (not agent_config.api_tokens)
        db.execute(
            """
            INSERT INTO tokens
            (token_id, type, provider, scope_user_id, scope_tenant_id, 
             encrypted_value, is_active, created_at)
            VALUES (%s, %s, %s, %s, %s, %s, TRUE, %s)
            """,
            (
                token_id,
                token_type,
                provider,
                scope_user_id,
                scope_tenant_id,
                token_value,
                datetime.now(),
            ),
        )

        # Audit log
        audit.log_token_create(user, token_type, provider, scope)

        click.echo("✅ Token created successfully")
        click.echo(f"   Token ID: {token_id}")
        click.echo(f"   Scope: {scope}" + (f" ({scope_id})" if scope_id else ""))
    except Exception as e:
        click.echo(f"❌ Failed to create token: {e}")
        sys.exit(1)


@token.command("list")
@click.option("--scope", type=click.Choice(["global", "account", "user"]))
@click.option("--scope-id")
@click.pass_context
def token_list(ctx, scope, scope_id):
    """List API tokens (values hidden)."""
    db = ctx.obj["db"]

    query = """
        SELECT token_id, type, provider, scope_user_id, scope_tenant_id, 
               is_active, created_at 
        FROM tokens
        WHERE 1=1
    """
    params = []

    if scope == "user" and scope_id:
        query += " AND scope_user_id = %s"
        params.append(scope_id)
    elif scope == "account" and scope_id:
        query += " AND scope_tenant_id = %s"
        params.append(scope_id)
    elif scope == "global":
        query += " AND scope_user_id IS NULL AND scope_tenant_id IS NULL"

    query += " ORDER BY created_at DESC"

    tokens = db.fetchall(query, tuple(params))

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
        elif t["scope_tenant_id"]:
            scope_str = f"account ({t['scope_tenant_id']})"
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


@cli.command()
def init():
    """Initialize agent system (database + RBAC)."""
    import subprocess

    click.echo("🔧 Initializing agent system...")

    result = subprocess.run(["make", "db-init-agent"], capture_output=True, text=True)

    if result.returncode == 0:
        click.echo("✅ Agent system initialized successfully")
    else:
        click.echo("❌ Initialization failed", err=True)
        click.echo(result.stderr, err=True)
        sys.exit(1)


if __name__ == "__main__":
    cli()
