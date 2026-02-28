#!/usr/bin/env python3
"""mo-agent CLI - API mode with sync wrapper."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent.parent))

import asyncio
import json
import logging
import click
from cli.api_client import APIClient

logger = logging.getLogger(__name__)


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
    
    def stream_agent_run_events(self, run_id: str):
        """Stream run events synchronously."""
        async def _stream():
            async with APIClient(base_url=self.base_url, profile=self.profile) as client:
                async for event in client.stream_agent_run_events(run_id):
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
@click.pass_context
def logout(ctx):
    """Logout — clear local tokens."""
    try:
        ctx.obj["client"].logout()
        click.echo("✅ Logged out")
    except Exception as e:
        click.echo(f"❌ {e}")


async def _run_edge_turn(user_input, sync_client, session_id, model, agent_id, auto_approve):
    """Run one edge chat loop turn using the async APIClient."""
    import os
    from cli.edge_chat_loop import edge_chat_loop
    from cli.permissions import PermissionManager
    from cli.tools.router import ToolRouter
    from cli.tools.file_ops import register_file_tools
    from cli.tools.shell import register_shell_tools
    from cli.tools.git import register_git_tools
    from cli.tools.search import register_search_tools

    project_root = os.getcwd()
    router = ToolRouter()
    register_file_tools(router, project_root)
    register_shell_tools(router, project_root)
    register_git_tools(router, project_root)
    register_search_tools(router, project_root)

    # Load local SKILL.md skills
    from core.skills.loader import SkillLoader
    builtin_names = {t.name for t in router.list_tools()}
    for local in SkillLoader.discover(SkillLoader.default_paths(project_root)):
        if local.skill.name in builtin_names:
            logger.warning(
                "Local skill '%s' skipped — conflicts with built-in tool", local.skill.name,
            )
            continue
        router.register(local.skill)

    # Introspection tool — session info populated with what we know at startup
    from cli.tools.introspection import GetAgentInfoTool
    session_info = {"session_id": session_id, "agent_id": agent_id, "model": model, "turn": 0}

    perms = PermissionManager(auto_approve=auto_approve)

    async with APIClient(base_url=sync_client.base_url, profile=sync_client.profile) as api:
        # Register introspection tool with api_client for cloud memory enrichment
        router.register(GetAgentInfoTool(tool_router=router, session_info=session_info, api_client=api))

        await edge_chat_loop(
            user_input, api, router, perms,
            session_id=session_id, project_root=project_root,
            model=model, agent_id=agent_id,
            session_info=session_info,
        )


@cli.command()
@click.option("--user-id", default="cli_user")
@click.option("--session-id", default=None)
@click.option("--model", default=None, help="Model to use for chat")
@click.option("--resume", is_flag=True, help="Resume last session")
@click.option("--auto-approve", is_flag=True, help="Auto-approve tool execution (dangerous commands still blocked)")
@click.option("--debug", is_flag=True, help="Print full traceback on errors")
@click.pass_context
def chat(ctx, user_id, session_id, model, resume, auto_approve, debug):
    """Start interactive chat with edge tool execution."""
    client = ctx.obj["client"]

    click.echo("🤖 mo-agent interactive chat")
    click.echo("=" * 50)

    auth_result = client.ensure_authenticated()
    if auth_result == "session_expired" or not auth_result:
        click.echo("❌ Not logged in")
        click.echo("")
        choice = click.prompt(
            "1) Login  2) Register new account  3) Exit",
            type=click.IntRange(1, 3), default=1,
        )
        if choice == 3:
            sys.exit(0)

        username = click.prompt("Username")
        password = click.prompt("Password", hide_input=True)

        if choice == 2:
            email = click.prompt("Email")
            try:
                client.register(username, password, email)
                click.echo(f"✅ Registered: {username}")
            except Exception as e:
                click.echo(f"❌ Registration failed: {e}")
                sys.exit(1)

        try:
            client.login(username, password)
            click.echo("✅ Logged in")
        except Exception as e:
            click.echo(f"❌ Login failed: {e}")
            sys.exit(1)

    try:
        user_info = client.get_current_user()
        username = user_info.get("username", "You")
    except Exception:
        username = "You"

    selected_model = model

    # Load persisted defaults from profile
    try:
        from cli.api_client import APIClient as _AC
        _profile = _AC.load_profile(profile=client.profile)
        _dm = _profile.get("default_model")
        _ls = _profile.get("last_session_id")
        if not selected_model and _dm:
            selected_model = _dm
        if resume and not session_id and _ls:
            session_id = _ls
            click.echo(f"🔄 Resuming session: {session_id}")
    except Exception:
        pass

    try:
        if not session_id:
            result = client.create_session(agent_id=user_id or "default-agent")
            session_id = result["session_id"]
            click.echo(f"📝 Session: {session_id}")
            if selected_model:
                click.echo(f"🤖 Model: {selected_model}")
            click.echo()

        # Persist last_session_id
        try:
            client.save_profile_setting(last_session_id=session_id)
        except Exception:
            pass

        while True:
            user_input = click.prompt(username, type=str, prompt_suffix="> ")

            if user_input.startswith("/"):
                cmd_parts = user_input.strip().split(maxsplit=1)
                cmd = cmd_parts[0].lower()
                cmd_arg = cmd_parts[1] if len(cmd_parts) > 1 else None

                if cmd in ("/exit", "/quit"):
                    break
                elif cmd == "/help":
                    click.echo("\n  /help           Show this help")
                    click.echo("  /model          List available models")
                    click.echo("  /model <name>   Select a model")
                    click.echo("  /session        Show current session info")
                    click.echo("  /clear          Start a new session")
                    click.echo("  /exit           Exit chat\n")
                elif cmd == "/model":
                    try:
                        models = client.admin_list_models()
                        active_models = [m for m in models if m.get("is_active", True)]
                        if cmd_arg:
                            if cmd_arg in [m["name"] for m in active_models]:
                                selected_model = cmd_arg
                                click.echo(f"\n✅ Model: {selected_model}\n")
                                try:
                                    client.save_profile_setting(default_model=selected_model)
                                except Exception:
                                    pass
                            else:
                                click.echo(f"\n❌ Unknown model '{cmd_arg}'")
                                if active_models:
                                    for m in active_models:
                                        click.echo(f"  • {m['name']} ({m['provider']})")
                                else:
                                    click.echo("  No active models. Run: mo-admin model add <name> <provider>")
                                click.echo()
                        else:
                            if not active_models:
                                click.echo("\n  No active models. Run: mo-admin model add <name> <provider>\n")
                            else:
                                for m in active_models:
                                    marker = "→" if selected_model == m["name"] else " "
                                    click.echo(f"  {marker} {m['name']} ({m['provider']})")
                                click.echo()
                    except Exception as e:
                        click.echo(f"\n❌ {e}\n")
                elif cmd == "/session":
                    click.echo(f"\n📝 {session_id}  👤 {username}  🤖 {selected_model or '(default)'}\n")
                elif cmd == "/clear":
                    try:
                        client.close_session(session_id)
                        result = client.create_session(agent_id=user_id or "default-agent")
                        session_id = result["session_id"]
                        click.echo(f"\n✅ New session: {session_id}\n")
                    except Exception as e:
                        click.echo(f"\n❌ {e}\n")
                else:
                    click.echo("❌ Unknown command. Type /help\n")
                continue

            if user_input.lower() in ("exit", "quit"):
                break

            try:
                asyncio.run(_run_edge_turn(
                    user_input, client, session_id,
                    selected_model, user_id, auto_approve,
                ))
            except Exception as e:
                if debug:
                    import traceback
                    click.echo(traceback.format_exc())
                else:
                    click.echo(f"\n❌ {type(e).__name__}: {e}")
            click.echo()

    except KeyboardInterrupt:
        click.echo("\n\nInterrupted")
    except RuntimeError as e:
        if "Session expired" in str(e):
            click.echo("\n❌ Session expired — please login again: mo-agent login")
            sys.exit(1)
        raise
    finally:
        try:
            client.close_session(session_id)
            click.echo("✅ Session closed")
        except Exception:
            pass


def require_auth(client):
    """Ensure user is authenticated."""
    result = client.ensure_authenticated()
    if result == "session_expired":
        click.echo("❌ Session expired — please login again: mo-agent login")
        sys.exit(1)
    if not result:
        click.echo("❌ Please login first: mo-agent login")
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
