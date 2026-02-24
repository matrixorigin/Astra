#!/usr/bin/env python3
"""mo-admin CLI - API mode with sync wrapper."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent.parent))

import asyncio
import json
from datetime import datetime, timedelta
import click
from cli.api_client import APIClient


class SyncAPIClient:
    """Synchronous wrapper for APIClient."""
    
    def __init__(self, base_url: str, profile: str | None = None):
        self.base_url = base_url
        self.profile = profile
    
    def _run(self, coro):
        return asyncio.run(coro)
    
    def __getattr__(self, name):
        async def wrapper(*args, **kwargs):
            async with APIClient(base_url=self.base_url, profile=self.profile) as client:
                method = getattr(client, name)
                return await method(*args, **kwargs)
        return lambda *args, **kwargs: self._run(wrapper(*args, **kwargs))


@click.group()
@click.option("--api-url", default="http://localhost:8000", envvar="MO_AGENT_API_URL")
@click.option("--profile", default=None, envvar="MO_AGENT_PROFILE", help="Profile to use")
@click.pass_context
@click.version_option(version="0.1.0")
def cli(ctx, api_url, profile):
    """mo-admin - Administrative interface."""
    ctx.ensure_object(dict)
    ctx.obj["client"] = SyncAPIClient(api_url, profile=profile)


@cli.command()
@click.option("--username", prompt=True)
@click.option("--password", prompt=True, hide_input=True)
@click.pass_context
def login(ctx, username, password):
    """Login as admin."""
    try:
        result = ctx.obj["client"].login(username, password)
        click.echo(f"✅ Logged in as {result.get('username', result.get('email', 'user'))}")
    except Exception as e:
        click.echo(f"❌ Login failed: {e}")
        sys.exit(1)


def require_auth(client):
    """Ensure user is authenticated."""
    if not client.ensure_authenticated():
        click.echo("❌ Please login first: mo-admin login")
        sys.exit(1)


@cli.command()
@click.pass_context
def init(ctx):
    """Initialize database."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.admin_init()
        click.echo("✅ Database initialized")
        click.echo(f"Tables created: {result.get('tables_created', 0)}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@cli.group()
def token():
    """Manage tokens."""


@token.command("create")
@click.option("--type", "token_type", type=click.Choice(["llm", "github", "api"]), required=True)
@click.option("--provider", required=True)
@click.option("--value", prompt=True, hide_input=True)
@click.option("--scope-type", default="global")
@click.option("--scope-id", default=None)
@click.option("--description", default=None)
@click.pass_context
def token_create(ctx, token_type, provider, value, scope_type, scope_id, description):
    """Create API token."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.admin_create_token(
            token_type=token_type,
            provider=provider,
            token_value=value,
            scope=scope_type,  # API expects 'scope', not 'scope_type'
            scope_id=scope_id,
        )
        click.echo(f"✅ Token created: {result['token_id']}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@token.command("list")
@click.option("--type", "token_type", default=None)
@click.option("--provider", default=None)
@click.option("--active-only", is_flag=True)
@click.pass_context
def token_list(ctx, token_type, provider, active_only):
    """List tokens."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        # API only accepts token_type and scope, not provider or active_only
        tokens = client.admin_list_tokens(
            token_type=token_type,
        )
        if not tokens:
            click.echo("No tokens found")
            return
        
        click.echo("API Tokens:")
        click.echo("=" * 100)
        for t in tokens:
            status = "🟢" if t.get("is_active") else "⚪"
            # API returns token_type, not type
            token_type_str = t.get('token_type', 'unknown')
            provider_str = t.get('provider', 'unknown')
            scope_str = t.get('scope', 'unknown')
            click.echo(f"{status} {t['token_id'][:8]}... | {token_type_str:8} | {provider_str:15} | {scope_str:10}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.group()
def audit():
    """View audit logs."""


@audit.command("logs")
@click.option("--user", default=None)
@click.option("--action", default=None)
@click.option("--since", default=None)
@click.option("--limit", default=50)
@click.pass_context
def audit_logs(ctx, user, action, since, limit):
    """View audit logs."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        since_date = None
        if since:
            since_date = datetime.strptime(since, "%Y-%m-%d").isoformat()
        
        # API only accepts user_id, since, limit - not action
        logs = client.admin_audit_logs(
            user_id=user,
            since=since_date,
            limit=limit,
        )
        if not logs:
            click.echo("No logs found")
            return
        
        click.echo("Audit Logs:")
        click.echo("=" * 100)
        for log in logs:
            click.echo(f"[{log.get('created_at')}] {log.get('action')} by {log.get('user_id')}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.group()
def feedback():
    """Manage feedback."""


@feedback.command("stats")
@click.option("--agent-id", default=None)
@click.option("--days", default=30)
@click.pass_context
def feedback_stats(ctx, agent_id, days):
    """Show feedback statistics."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        since = (datetime.now() - timedelta(days=days)).isoformat()
        stats = client.admin_feedback_stats(agent_id=agent_id, since=since)
        
        click.echo("Feedback Statistics:")
        click.echo("=" * 80)
        click.echo(f"Total: {stats.get('total', 0)}")
        click.echo(f"Positive (≥4): {stats.get('positive', 0)}")
        click.echo(f"Negative (<3): {stats.get('negative', 0)}")
        click.echo(f"Average Rating: {stats.get('avg_rating', 0):.2f}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@feedback.command("export")
@click.option("--output", type=click.Path(), default="feedback_export.jsonl")
@click.option("--min-rating", default=4)
@click.option("--days", default=30)
@click.pass_context
def feedback_export(ctx, output, min_rating, days):
    """Export feedback (async job)."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        # API only accepts agent_id and format, not min_rating or since
        result = client.admin_feedback_export()
        
        # API returns async job info, not data
        job_id = result.get("job_id")
        status = result.get("status")
        download_url = result.get("download_url")
        
        if download_url:
            click.echo(f"✅ Export ready: {download_url}")
        else:
            click.echo(f"📋 Export job created: {job_id}")
            click.echo(f"   Status: {status}")
            click.echo(f"   Check job status with: mo-admin job status {job_id}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.group()
def model():
    """Manage models."""


@model.command("add")
@click.argument("model_name")
@click.argument("provider")
@click.option("--scope", default="global")
@click.option("--scope-id", default=None)
@click.pass_context
def model_add(ctx, model_name, provider, scope, scope_id):
    """Register a model."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.admin_create_model(
            model_name=model_name,
            provider=provider,
            scope=scope,
            scope_id=scope_id,
        )
        click.echo(f"✅ Model registered: {model_name}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@model.command("list")
@click.pass_context
def model_list(ctx):
    """List models."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        models = client.admin_list_models()
        if not models:
            click.echo("No models found")
            return
        
        click.echo("Models:")
        for m in models:
            click.echo(f"  {m['name']} ({m['provider']})")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.command()
@click.option("--username", prompt=True)
@click.option("--password", prompt=True, hide_input=True)
@click.option("--email", default=None)
@click.pass_context
def register(ctx, username, password, email):
    """Register new admin user."""
    client = ctx.obj["client"]
    
    try:
        result = client.admin_register(
            username=username,
            password=password,
            email=email or f"{username}@admin.local",
        )
        click.echo(f"✅ Admin registered: {username}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@cli.command()
@click.pass_context
def whoami(ctx):
    """Show current user info."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.get_current_user()
        if "username" in result:
            click.echo(f"Username: {result['username']}")
        click.echo(f"Email: {result['email']}")
        if "user_id" in result:
            click.echo(f"User ID: {result['user_id']}")
        if "role" in result:
            click.echo(f"Role: {result['role']}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")
        sys.exit(1)


@cli.group()
def user():
    """Manage users."""


@user.command("grant-role")
@click.argument("username")
@click.argument("role_name", type=click.Choice(["mo_agent_admin", "mo_agent_user"]))
@click.pass_context
def user_grant_role(ctx, username, role_name):
    """Grant a role to a user."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.admin_grant_role(username=username, role_name=role_name)
        click.echo(f"✅ {result.get('message', 'Role granted')}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@user.command("revoke-role")
@click.argument("username")
@click.argument("role_name", type=click.Choice(["mo_agent_admin", "mo_agent_user"]))
@click.pass_context
def user_revoke_role(ctx, username, role_name):
    """Revoke a role from a user."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.admin_revoke_role(username=username, role_name=role_name)
        click.echo(f"✅ {result.get('message', 'Role revoked')}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


if __name__ == "__main__":
    cli()
