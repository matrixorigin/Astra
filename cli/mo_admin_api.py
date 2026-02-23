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
    
    def __init__(self, base_url: str):
        self.base_url = base_url
    
    def _run(self, coro):
        return asyncio.run(coro)
    
    def __getattr__(self, name):
        async def wrapper(*args, **kwargs):
            async with APIClient(base_url=self.base_url) as client:
                method = getattr(client, name)
                return await method(*args, **kwargs)
        return lambda *args, **kwargs: self._run(wrapper(*args, **kwargs))


@click.group()
@click.option("--api-url", default="http://localhost:8000", envvar="MO_AGENT_API_URL")
@click.pass_context
@click.version_option(version="0.1.0")
def cli(ctx, api_url):
    """mo-admin - Administrative interface."""
    ctx.ensure_object(dict)
    ctx.obj["client"] = SyncAPIClient(api_url)


@cli.command()
@click.option("--email", prompt=True)
@click.option("--password", prompt=True, hide_input=True)
@click.pass_context
def login(ctx, email, password):
    """Login as admin."""
    try:
        result = ctx.obj["client"].login(email, password)
        click.echo(f"✅ Logged in as {result['email']}")
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
            scope_type=scope_type,
            scope_id=scope_id,
            description=description,
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
        tokens = client.admin_list_tokens(
            token_type=token_type,
            provider=provider,
            active_only=active_only,
        )
        if not tokens:
            click.echo("No tokens found")
            return
        
        click.echo("API Tokens:")
        click.echo("=" * 100)
        for t in tokens:
            status = "🟢" if t.get("is_active") else "⚪"
            click.echo(f"{status} {t['token_id'][:8]}... | {t['type']:8} | {t['provider']:15} | {t['scope_type']:10}")
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
        
        logs = client.admin_audit_logs(
            user_id=user,
            action=action,
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
    """Export feedback."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        since = (datetime.now() - timedelta(days=days)).isoformat()
        result = client.admin_feedback_export(min_rating=min_rating, since=since)
        
        with open(output, "w") as f:
            for item in result.get("data", []):
                f.write(json.dumps(item) + "\n")
        
        click.echo(f"✅ Exported {result.get('count', 0)} items to {output}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.command()
@click.pass_context
def whoami(ctx):
    """Show current user."""
    try:
        user = ctx.obj["client"].get_current_user()
        click.echo(f"Logged in as: {user['email']}")
        click.echo(f"Role: {user.get('role', 'N/A')}")
    except Exception as e:
        click.echo(f"❌ Not authenticated: {e}")


if __name__ == "__main__":
    cli()
