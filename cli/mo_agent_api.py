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
    
    def __init__(self, base_url: str, profile: str | None = None):
        self.base_url = base_url
        self.profile = profile
    
    def _run(self, coro):
        """Run async coroutine synchronously."""
        try:
            # Try to get existing event loop
            loop = asyncio.get_running_loop()
            # If we're in an async context, we can't use asyncio.run()
            # This is a limitation - we need to be in sync context
            raise RuntimeError("Cannot run sync client in async context")
        except RuntimeError:
            # No running loop, safe to use asyncio.run()
            return asyncio.run(coro)
    
    def chat_stream(self, message: str, session_id: str | None = None, agent_id: str | None = None, model: str | None = None):
        """Stream chat response synchronously."""
        async def _stream():
            async with APIClient(base_url=self.base_url, profile=self.profile) as client:
                async for chunk in client.chat_stream(message, session_id=session_id, agent_id=agent_id, model=model):
                    yield chunk
        
        # Run async generator synchronously
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            gen = _stream()
            while True:
                try:
                    yield loop.run_until_complete(gen.__anext__())
                except StopAsyncIteration:
                    break
        finally:
            loop.close()
    
    def stream_run_events(self, run_id: str):
        """Stream run events synchronously."""
        async def _stream():
            async with APIClient(base_url=self.base_url, profile=self.profile) as client:
                async for event in client.stream_run_events(run_id):
                    yield event
        
        # Run async generator synchronously
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            gen = _stream()
            while True:
                try:
                    yield loop.run_until_complete(gen.__anext__())
                except StopAsyncIteration:
                    break
        finally:
            loop.close()
    
    def __getattr__(self, name):
        """Wrap all async methods to run synchronously."""
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
    """mo-agent - Event-centric intelligent agent platform."""
    ctx.ensure_object(dict)
    ctx.obj["client"] = SyncAPIClient(api_url, profile=profile)
    ctx.obj["profile"] = profile


@cli.command()
@click.option("--username", prompt=True)
@click.option("--password", prompt=True, hide_input=True)
@click.pass_context
def login(ctx, username, password):
    """Login to mo-agent API."""
    try:
        result = ctx.obj["client"].login(username, password)
        click.echo(f"✅ Logged in as {result.get('username', result.get('email', 'user'))}")
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
        error_msg = str(e)
        # Extract detail from API error if available
        if "Username already exists" in error_msg:
            click.echo(f"❌ Username '{username}' is already taken")
        elif "Email already exists" in error_msg:
            click.echo(f"❌ Email '{email}' is already registered")
        elif "String should have at least 8 characters" in error_msg and "password" in error_msg:
            click.echo(f"❌ Password must be at least 8 characters")
        elif "422" in error_msg:
            # Parse validation error
            if "password" in error_msg:
                click.echo(f"❌ Invalid password format")
            elif "email" in error_msg:
                click.echo(f"❌ Invalid email format")
            else:
                click.echo(f"❌ Invalid input")
        else:
            click.echo(f"❌ Registration failed: {error_msg}")
        sys.exit(1)


@cli.command()
@click.option("--user-id", default="cli_user")
@click.option("--session-id", default=None)
@click.option("--no-stream", is_flag=True, help="Disable streaming output")
@click.option("--model", default=None, help="Model to use for chat")
@click.pass_context
def chat(ctx, user_id, session_id, no_stream, model):
    """Start interactive chat with streaming support."""
    client = ctx.obj["client"]
    
    click.echo("🤖 mo-agent interactive chat")
    click.echo("=" * 50)
    
    if not client.ensure_authenticated():
        click.echo("❌ Not logged in")
        click.echo("")
        if click.confirm("Login now?", default=True):
            username = click.prompt("Username")
            password = click.prompt("Password", hide_input=True)
            try:
                client.login(username, password)
                click.echo("✅ Logged in")
            except Exception as e:
                click.echo(f"❌ Login failed: {e}")
                sys.exit(1)
        else:
            click.echo("Run: mo-agent login")
            sys.exit(1)
    
    # Get current user info
    try:
        user_info = client.get_current_user()
        username = user_info.get("username", "You")
    except Exception:
        username = "You"
    
    # Track selected model
    selected_model = model
    
    if not session_id:
        # API expects agent_id, not user_id
        # Use user_id as agent_id for now (or use default agent)
        result = client.create_session(agent_id=user_id or "default-agent")
        session_id = result["session_id"]
        click.echo(f"📝 Session: {session_id}")
        if selected_model:
            click.echo(f"🤖 Model: {selected_model}")
        click.echo()
    
    try:
        while True:
            user_input = click.prompt(username, type=str, prompt_suffix="> ")
            
            # Handle slash commands
            if user_input.startswith("/"):
                cmd_parts = user_input.strip().split(maxsplit=1)
                cmd = cmd_parts[0].lower()
                cmd_arg = cmd_parts[1] if len(cmd_parts) > 1 else None
                
                if cmd in ["/exit", "/quit"]:
                    break
                elif cmd == "/help":
                    click.echo("\nAvailable commands:")
                    click.echo("  /help           - Show this help")
                    click.echo("  /model          - List available models")
                    click.echo("  /model <name>   - Select a model")
                    click.echo("  /session        - Show current session info")
                    click.echo("  /clear          - Start a new session")
                    click.echo("  /exit           - Exit chat")
                    click.echo()
                    continue
                elif cmd == "/model":
                    try:
                        models = client.admin_list_models()
                        if models:
                            if cmd_arg:
                                # Select model
                                model_names = [m['name'] for m in models]
                                if cmd_arg in model_names:
                                    selected_model = cmd_arg
                                    click.echo(f"\n✅ Model selected: {selected_model}\n")
                                else:
                                    click.echo(f"\n❌ Model '{cmd_arg}' not found")
                                    click.echo("Available models:")
                                    for m in models:
                                        click.echo(f"  • {m['name']} ({m['provider']})")
                                    click.echo()
                            else:
                                # List models
                                click.echo("\nAvailable models:")
                                for m in models:
                                    marker = "→" if selected_model == m['name'] else " "
                                    click.echo(f"  {marker} {m['name']} ({m['provider']})")
                                click.echo()
                                if selected_model:
                                    click.echo(f"Current: {selected_model}")
                                else:
                                    click.echo("To select: /model <name>")
                                click.echo()
                        else:
                            click.echo("\n⚠️  No models configured")
                            click.echo("Configure models: make dev-setup-demo → Configure models")
                            click.echo()
                    except Exception as e:
                        click.echo(f"\n❌ Failed to list models: {e}\n")
                    continue
                elif cmd == "/session":
                    click.echo(f"\n📝 Session ID: {session_id}")
                    click.echo(f"👤 User: {username}")
                    if selected_model:
                        click.echo(f"🤖 Model: {selected_model}")
                    else:
                        click.echo(f"🤖 Model: (default)")
                    click.echo()
                    continue
                elif cmd == "/clear":
                    try:
                        client.close_session(session_id)
                        result = client.create_session(agent_id=user_id or "default-agent")
                        session_id = result["session_id"]
                        click.echo(f"\n✅ New session: {session_id}\n")
                    except Exception as e:
                        click.echo(f"\n❌ Failed to create new session: {e}\n")
                    continue
                else:
                    click.echo(f"\n❌ Unknown command: {user_input}")
                    click.echo("Type /help for available commands\n")
                    continue
            
            if user_input.lower() in ["exit", "quit"]:
                break
            
            if no_stream:
                # Non-streaming mode: poll for result
                result = client.chat(user_input, session_id=session_id, model=selected_model)
                run_id = result.get("run_id")
                
                # Poll for completion
                import time
                while True:
                    status = client.get_run_status(run_id)
                    if status["status"] in ["completed", "failed", "cancelled"]:
                        break
                    time.sleep(0.5)
                
                # Get final response from run events
                if status["status"] == "completed":
                    # Stream events to get response
                    response_text = ""
                    for event in client.stream_run_events(run_id):
                        if event.get("type") == "content":
                            response_text += event.get("content", "")
                    click.echo(f"Agent> {response_text}\n")
                else:
                    click.echo(f"❌ Run {status['status']}\n")
            else:
                # Streaming mode
                click.echo("Agent> ", nl=False)
                for chunk in client.chat_stream(user_input, session_id=session_id, model=selected_model):
                    if chunk.get("type") == "content":
                        click.echo(chunk.get("content", ""), nl=False)
                    elif chunk.get("type") == "done":
                        click.echo()  # Newline after completion
                click.echo()  # Extra newline for spacing
    
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
    try:
        if not client.ensure_authenticated():
            click.echo("❌ Please login first: mo-agent login")
            sys.exit(1)
    except Exception as e:
        click.echo(f"❌ Authentication check failed: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)


@cli.group()
def session():
    """Manage sessions."""


@session.command("list")
@click.option("--limit", default=20)
@click.pass_context
def session_list(ctx, limit):
    """List sessions."""
    client = ctx.obj["client"]
    require_auth(client)
    
    try:
        result = client.list_sessions(limit=limit)
        # API returns {"sessions": [...], "total": ...}
        sessions = result.get("sessions", []) if isinstance(result, dict) else result
        
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


@cli.group()
def profile():
    """Manage user profiles."""
    pass


@profile.command("list")
def profile_list():
    """List all profiles."""
    from cli.profile_manager import ProfileManager
    
    manager = ProfileManager()
    profiles = manager.list_profiles()
    
    if not profiles:
        click.echo("No profiles found")
        return
    
    click.echo("Profiles:")
    for p in profiles:
        marker = "* " if p["current"] else "  "
        click.echo(f"{marker}{p['name']} ({p['username']})")


@profile.command("use")
@click.argument("profile_name")
def profile_use(profile_name):
    """Switch to a different profile."""
    from cli.profile_manager import ProfileManager
    
    manager = ProfileManager()
    try:
        manager.set_current_profile(profile_name)
        click.echo(f"✅ Switched to profile: {profile_name}")
    except ValueError as e:
        click.echo(f"❌ {e}")
        sys.exit(1)


@profile.command("delete")
@click.argument("profile_name")
@click.confirmation_option(prompt="Are you sure you want to delete this profile?")
def profile_delete(profile_name):
    """Delete a profile."""
    from cli.profile_manager import ProfileManager
    
    manager = ProfileManager()
    try:
        manager.delete_profile(profile_name)
        click.echo(f"✅ Deleted profile: {profile_name}")
    except ValueError as e:
        click.echo(f"❌ {e}")
        sys.exit(1)


if __name__ == "__main__":
    cli()
