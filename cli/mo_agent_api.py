#!/usr/bin/env python3
"""mo-agent CLI - API mode with sync wrapper."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent.parent))

import asyncio
import json
import click
from cli.api_client import APIClient


class SyncAPIClient:
    """Synchronous wrapper for APIClient."""
    
    def __init__(self, base_url: str):
        self.base_url = base_url
    
    def _run(self, coro):
        """Run async coroutine synchronously."""
        return asyncio.run(coro)
    
    def __getattr__(self, name):
        """Wrap all async methods to run synchronously."""
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
    """mo-agent - Event-centric intelligent agent platform."""
    ctx.ensure_object(dict)
    ctx.obj["client"] = SyncAPIClient(api_url)


@cli.command()
@click.option("--email", prompt=True)
@click.option("--password", prompt=True, hide_input=True)
@click.pass_context
def login(ctx, email, password):
    """Login to mo-agent API."""
    try:
        result = ctx.obj["client"].login(email, password)
        click.echo(f"✅ Logged in as {result['email']}")
    except Exception as e:
        click.echo(f"❌ Login failed: {e}")
        sys.exit(1)


@cli.command()
@click.option("--email", prompt=True)
@click.option("--password", prompt=True, hide_input=True, confirmation_prompt=True)
@click.option("--username", prompt=True)
@click.pass_context
def register(ctx, email, password, username):
    """Register new user."""
    try:
        result = ctx.obj["client"].register(username, password, email)
        click.echo(f"✅ Registered as {result['email']}")
    except Exception as e:
        click.echo(f"❌ Registration failed: {e}")
        sys.exit(1)


@cli.command()
@click.option("--user-id", default="cli_user")
@click.option("--session-id", default=None)
@click.pass_context
def chat(ctx, user_id, session_id):
    """Start interactive chat (streaming not yet supported in sync mode)."""
    client = ctx.obj["client"]
    
    click.echo("🤖 mo-agent interactive chat")
    click.echo("=" * 50)
    
    if not client.ensure_authenticated():
        click.echo("❌ Please login first: mo-agent login")
        sys.exit(1)
    
    if not session_id:
        result = client.create_session(user_id=user_id)
        session_id = result["session_id"]
        click.echo(f"📝 Session: {session_id}\n")
    
    try:
        while True:
            user_input = click.prompt("You", type=str, prompt_suffix="> ")
            if user_input.lower() in ["exit", "quit"]:
                break
            
            result = client.chat(user_input, session_id=session_id)
            click.echo(f"Agent> {result.get('response', '')}\n")
    
    except KeyboardInterrupt:
        click.echo("\n\nInterrupted")
    finally:
        try:
            client.close_session(session_id)
            click.echo(f"✅ Session closed")
        except Exception:
            pass


def require_auth(client):
    """Ensure user is authenticated."""
    if not client.ensure_authenticated():
        click.echo("❌ Please login first: mo-agent login")
        sys.exit(1)


@cli.group()
def session():
    """Manage sessions."""


@session.command("list")
@click.option("--user-id", default=None)
@click.option("--limit", default=20)
@click.pass_context
def session_list(ctx, user_id, limit):
    """List sessions."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        sessions = client.list_sessions(user_id=user_id, limit=limit)
        if not sessions:
            click.echo("No sessions found")
            return
        
        click.echo("Recent Sessions:")
        click.echo("=" * 80)
        for s in sessions:
            status = "🟢" if s.get("status") == "active" else "⚪"
            click.echo(f"{status} {s['session_id']} | {s.get('user_id', 'N/A')} | {s.get('event_count', 0)} events")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@session.command("show")
@click.argument("session_id")
@click.pass_context
def session_show(ctx, session_id):
    """Show session details."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        s = client.get_session(session_id)
        click.echo(f"Session: {s['session_id']}")
        click.echo(f"User: {s.get('user_id', 'N/A')}")
        click.echo(f"Status: {s.get('status', 'N/A')}")
        click.echo(f"Events: {s.get('event_count', 0)}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.group()
def skill():
    """Manage skills."""


@skill.command("list")
@click.option("--active-only", is_flag=True)
@click.pass_context
def skill_list(ctx, active_only):
    """List skills."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        skills = client.list_skills(active_only=active_only)
        if not skills:
            click.echo("No skills found")
            return
        
        click.echo("Available Skills:")
        click.echo("=" * 70)
        for skill in skills:
            status = "✓" if skill.get("is_active") else "✗"
            click.echo(f"{status} {skill['skill_name']} v{skill['version']}")
            click.echo(f"  {skill.get('description', '')}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@skill.command("register")
@click.argument("skill_file", type=click.Path(exists=True))
@click.pass_context
def skill_register(ctx, skill_file):
    """Register skill from file."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        with open(skill_file) as f:
            skill_data = json.load(f)
        result = client.register_skill(skill_data)
        click.echo(f"✅ Registered: {result['skill_name']} v{result['version']}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.command()
@click.argument("session_id")
@click.pass_context
def replay(ctx, session_id):
    """Replay session."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        click.echo(f"🔄 Replaying {session_id}...")
        result = client.replay_session(session_id)
        click.echo(f"✅ Replayed {result.get('events_replayed', 0)} events")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@cli.command()
@click.pass_context
def whoami(ctx):
    """Show current user."""
    try:
        user = ctx.obj["client"].get_current_user()
        click.echo(f"Logged in as: {user['email']}")
        click.echo(f"User ID: {user['user_id']}")
    except Exception as e:
        click.echo(f"❌ Not authenticated: {e}")


if __name__ == "__main__":
    cli()
