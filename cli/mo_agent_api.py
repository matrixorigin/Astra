#!/usr/bin/env python3
"""mo-agent CLI - API mode with rich terminal UX."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent.parent))

import asyncio
import json
import logging
import subprocess
import click
from cli.api_client import APIClient, AuthenticationError

logger = logging.getLogger(__name__)

VERSION = "0.1.0"


class SyncAPIClient:
    """Synchronous wrapper for APIClient.

    Holds a single long-lived APIClient so tokens stay in memory across calls.
    The old design created a new APIClient per call, relying on the credentials
    file as the only shared state — any write inconsistency caused auth failures.
    """

    def __init__(self, base_url: str, profile: str | None = None):
        self.base_url = base_url
        self.profile = profile
        self._api: APIClient | None = None
        self._loop: asyncio.AbstractEventLoop | None = None

    def _ensure_client(self) -> APIClient:
        """Lazily create and enter the persistent APIClient."""
        if self._api is None:
            self._loop = asyncio.new_event_loop()
            self._api = APIClient(base_url=self.base_url, profile=self.profile)
            self._loop.run_until_complete(self._api.__aenter__())
        return self._api

    def close(self) -> None:
        """Clean up the persistent client."""
        if self._api and self._loop:
            try:
                self._loop.run_until_complete(self._api.__aexit__(None, None, None))
            except Exception:
                pass
        self._api = None
        if self._loop:
            self._loop.close()
            self._loop = None

    def _run(self, coro):
        self._ensure_client()
        assert self._loop is not None
        return self._loop.run_until_complete(coro)

    def chat_stream(self, message, session_id=None, agent_id=None, model=None):
        api = self._ensure_client()
        assert self._loop is not None
        gen = api.chat_stream(message, session_id=session_id, agent_id=agent_id, model=model)
        try:
            while True:
                try:
                    yield self._loop.run_until_complete(gen.__anext__())
                except StopAsyncIteration:
                    break
        finally:
            pass

    def stream_agent_run_events(self, run_id):
        api = self._ensure_client()
        assert self._loop is not None
        gen = api.stream_agent_run_events(run_id)
        try:
            while True:
                try:
                    yield self._loop.run_until_complete(gen.__anext__())
                except StopAsyncIteration:
                    break
        finally:
            pass

    def __getattr__(self, name):
        def wrapper(*args, **kwargs):
            api = self._ensure_client()
            method = getattr(api, name)
            return self._run(method(*args, **kwargs))
        return wrapper


# ============================================================================
# Slash command handlers (used by chat REPL)
# ============================================================================

def _get_console():
    """Lazy import to avoid import cost for non-chat commands."""
    from rich.console import Console
    return Console(stderr=True)


def cmd_help(console, **_):
    from rich.table import Table
    t = Table(show_header=False, box=None, padding=(0, 2))
    t.add_column(style="cyan")
    t.add_column(style="dim")
    for cmd, desc in [
        ("/help", "Show this help"),
        ("/model", "List available models"),
        ("/model <name>", "Select a model"),
        ("/session", "Show current session info"),
        ("/clear", "Start a new session"),
        ("/login", "Login to API"),
        ("/logout", "Logout"),
        ("/skill", "List local skills"),
        ("/skill new <name>", "Create a new skill"),
        ("/skill test <name>", "Test a skill with full output"),
        ("/skill dev <name>", "Enter AI-assisted skill dev mode"),
        ("/verbose", "Show status bar"),
        ("/compact", "Hide status bar"),
        ("/history", "Show recent turns"),
        ("/copy", "Copy last response to clipboard"),
        ("/doctor", "Run diagnostics"),
        ("/version", "Show version info"),
        ("/exit", "Exit chat"),
    ]:
        t.add_row(cmd, desc)
    console.print(t)


def cmd_model(console, client=None, selected_model=None, cmd_arg=None, state=None, **_):
    from cli.api_client import AuthenticationError
    try:
        models = client.admin_list_models()
        active = [m for m in models if m.get("is_active", True)]
        if cmd_arg:
            if cmd_arg in [m["name"] for m in active]:
                state["selected_model"] = cmd_arg
                console.print(f"[green]✓[/green] Model: {cmd_arg}")
                try:
                    client.save_profile_setting(default_model=cmd_arg)
                except Exception:
                    pass
            else:
                console.print(f"[red]✗[/red] Unknown model '{cmd_arg}'")
                for m in active:
                    console.print(f"  • {m['name']} ({m['provider']})", style="dim")
        else:
            if not active:
                console.print("No active models. Run: mo-admin model add <name> <provider>", style="dim")
            else:
                from rich.table import Table
                t = Table(show_header=True, box=None)
                t.add_column("", width=2)
                t.add_column("Model")
                t.add_column("Provider", style="dim")
                for m in active:
                    marker = "→" if state.get("selected_model") == m["name"] else ""
                    t.add_row(marker, m["name"], m["provider"])
                console.print(t)
    except AuthenticationError:
        console.print("[red]✗[/red] Session expired — please login again: mo-agent login")
    except Exception as e:
        console.print(f"[red]✗[/red] {e}")


def cmd_session(console, session_id=None, username=None, state=None, **_):
    from rich.panel import Panel
    model = state.get("selected_model", "(default)") if state else "(default)"
    console.print(Panel(
        f"📝 {session_id}\n👤 {username}\n🤖 {model}",
        title="Session", border_style="bright_black",
    ))


def cmd_clear(console, client=None, user_id=None, state=None, **_):
    try:
        old_sid = state.get("session_id")
        if old_sid:
            client.close_session(old_sid)
        result = client.create_session(agent_id=user_id or "default-agent")
        state["session_id"] = result["session_id"]
        console.print(f"[green]✓[/green] New session: {state['session_id']}")
    except Exception as e:
        console.print(f"[red]✗[/red] {e}")


def cmd_verbose(console, status_bar=None, **_):
    if status_bar:
        status_bar.verbose = True
        console.print("[green]✓[/green] Status bar enabled")


def cmd_compact(console, status_bar=None, **_):
    if status_bar:
        status_bar.verbose = False
        console.print("[green]✓[/green] Status bar hidden")


def cmd_history(console, state=None, **_):
    turns = state.get("turn_history", []) if state else []
    if not turns:
        console.print("No history yet", style="dim")
        return
    for i, t in enumerate(turns[-10:], 1):
        console.print(f"  {i}. [cyan]{t['role']}[/cyan]: {t['preview']}")


def cmd_copy(console, state=None, **_):
    last = state.get("last_response", "") if state else ""
    if not last:
        console.print("Nothing to copy", style="dim")
        return
    for tool in ["pbcopy", "xclip", "xsel"]:
        try:
            subprocess.run([tool], input=last.encode(), check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            console.print("[green]✓[/green] Copied to clipboard")
            return
        except (FileNotFoundError, subprocess.CalledProcessError):
            continue
    console.print("[yellow]No clipboard tool found (pbcopy/xclip/xsel)[/yellow]")


def cmd_doctor(console, client=None, **_):
    from cli.ui.doctor import run_doctor
    run_doctor(console, client)


def cmd_version(console, **_):
    import platform
    from importlib.metadata import version as pkg_version
    console.print(f"mo-agent {VERSION}")
    console.print(f"Python {platform.python_version()}", style="dim")
    try:
        console.print(f"rich {pkg_version('rich')}", style="dim")
    except Exception:
        pass
    try:
        console.print(f"prompt_toolkit {pkg_version('prompt_toolkit')}", style="dim")
    except Exception:
        pass


def cmd_login(console, client=None, **_):
    import click as _click
    username = _click.prompt("Username")
    password = _click.prompt("Password", hide_input=True)
    try:
        client.login(username, password)
        console.print(f"[green]✓[/green] Logged in as {username}")
    except Exception as e:
        console.print(f"[red]✗[/red] Login failed: {e}")


def cmd_logout(console, client=None, **_):
    try:
        client.logout()
        console.print("[green]✓[/green] Logged out — use [cyan]/login[/cyan] to re-authenticate")
    except Exception as e:
        console.print(f"[red]✗[/red] {e}")


def cmd_skill(console, cmd_arg=None, client=None, state=None, **kw):
    """List, test, create, or develop local skills."""
    import os
    from core.skills.loader import SkillLoader

    project_root = os.getcwd()
    skills_root = Path(project_root) / ".mo-agent" / "skills"

    parts = (cmd_arg or "").split(maxsplit=1)
    sub = parts[0] if parts else "list"
    rest = parts[1] if len(parts) > 1 else ""

    # --- /skill list (default) ---
    if sub in ("list", ""):
        skills = SkillLoader.discover(SkillLoader.default_paths(project_root))
        if not skills:
            console.print("[dim]No local skills found in .mo-agent/skills/[/dim]")
            return
        from core.skills.markdown_skill import MarkdownSkill
        from rich.table import Table
        t = Table(show_header=True, box=None)
        t.add_column("Skill")
        t.add_column("Type", style="dim")
        t.add_column("Version", style="dim")
        t.add_column("Description", style="dim")
        for s in skills:
            kind = "md" if isinstance(s.skill, MarkdownSkill) else "py"
            t.add_row(s.skill.name, kind, s.skill.version, s.skill.description[:50])
        console.print(t)
        return

    # --- /skill new <name> ---
    if sub == "new":
        name = _normalize_skill_name(rest.strip())
        if not name:
            console.print("[red]Usage: /skill new <name>[/red]")
            return
        skill_dir = skills_root / _to_slug(name)
        if skill_dir.exists():
            console.print(f"[red]✗[/red] {skill_dir} already exists")
            return
        skill_dir.mkdir(parents=True)
        cls = _to_class(name)
        (skill_dir / "skill.py").write_text(
            f'''from core.skills.base import Skill, SkillInput, SkillOutput
from pydantic import Field


class {cls}Input(SkillInput):
    query: str = Field(description="User query")


class {cls}Output(SkillOutput):
    data: dict = {{}}


class {cls}Skill(Skill[{cls}Input, {cls}Output]):
    name = "{name}"
    version = "1.0.0"
    description = "TODO: describe what this skill does"

    async def execute(self, input: {cls}Input) -> {cls}Output:
        # TODO: implement
        return {cls}Output(success=True, data={{"query": input.query}})
''')
        (skill_dir / "SKILL.md").write_text(
            f'''---
name: {name}
version: 1.0.0
description: TODO
---

# {name}

TODO: describe this skill for the LLM.
''')
        console.print(f"[green]✓[/green] Created {skill_dir.relative_to(project_root)}/")
        console.print("  [dim]skill.py[/dim]  — implement your logic here")
        console.print("  [dim]SKILL.md[/dim]  — LLM-facing description (fallback if no skill.py)")
        console.print(f"\n  Test: [cyan]/skill test {name}[/cyan]")
        console.print(f"  Dev:  [cyan]/skill dev {name}[/cyan]")
        return

    # --- /skill test <name> [json_args] ---
    if sub == "test":
        test_parts = rest.split(maxsplit=1)
        name = test_parts[0] if test_parts else None
        raw_args = test_parts[1] if len(test_parts) > 1 else None
        if not name:
            console.print("[red]Usage: /skill test <name> [json_args][/red]")
            console.print('[dim]  e.g. /skill test stock_basic_info {"stock_code": "300355"}[/dim]')
            return
        skills = SkillLoader.discover(SkillLoader.default_paths(project_root))
        match = next((s for s in skills if s.skill.name == name), None)
        if not match:
            console.print(f"[red]✗[/red] Skill '{name}' not found")
            return

        if raw_args:
            try:
                args = json.loads(raw_args)
            except json.JSONDecodeError:
                args = {"query": raw_args}
        else:
            args = {"query": "test"}

        console.print(f"[bold]{name}[/bold] v{match.skill.version}")
        schema = match.skill.to_openai_schema()
        params = schema["function"]["parameters"].get("properties", {})
        console.print(f"  params: {', '.join(params.keys()) or '(none)'}")
        console.print(f"  input:  {json.dumps(args, ensure_ascii=False)}")

        try:
            from cli.tools.router import ToolRouter, ToolCall
            router = ToolRouter()
            router.register(match.skill)
            results = asyncio.run(router.execute([ToolCall(id="test", name=name, arguments=args)]))
            r = results[0]
            console.print(f"  time:   {r.execution_time_ms}ms")
            if r.error:
                console.print(f"  [red]✗ ERROR[/red]: {r.result}")
            else:
                # Parse once, display + validate from the same parsed data
                try:
                    data = json.loads(r.result)
                    formatted = json.dumps(data, indent=2, ensure_ascii=False)
                    console.print(f"  [green]✓ OUTPUT[/green] ({len(r.result)} chars):")
                    for line in formatted.split("\n"):
                        console.print(f"    [dim]{line}[/dim]")
                    for w in _validate_skill_output(data):
                        console.print(f"  [yellow]⚠ {w}[/yellow]")
                except json.JSONDecodeError:
                    console.print(f"  [green]✓ OUTPUT[/green]: {r.result[:500]}")
        except Exception as e:
            console.print(f"  [red]✗ EXCEPTION[/red]: {e}")
        return

    # --- /skill dev off ---
    if sub == "dev" and rest.strip() == "off":
        if state:
            state.pop("skill_dev_context", None)
            state.pop("skill_dev_name", None)
            state.pop("skill_dev_dir", None)
        console.print("[dim]Exited skill dev mode[/dim]")
        return

    # --- /skill dev <name> ---
    if sub == "dev":
        name = _normalize_skill_name(rest.strip())
        if not name:
            console.print("[red]Usage: /skill dev <name> | /skill dev off[/red]")
            return
        skill_dir = skills_root / _to_slug(name)
        if not skill_dir.exists():
            console.print(f"[red]✗[/red] Skill '{name}' not found. Create it first: [cyan]/skill new {name}[/cyan]")
            return
        # state=None means cmd_skill was called outside the chat REPL (e.g. tests
        # that don't pass state).  Still print success so the user sees feedback.
        state_name = _to_slug(name)  # store slug form — matches directory name
        if state is not None:
            state["skill_dev_name"] = state_name
            state["skill_dev_dir"] = str(skill_dir)
            state["skill_dev_context"] = _build_skill_dev_context(name, skill_dir)
        console.print(f"[green]✓[/green] Entered dev mode for [bold]{name}[/bold]")
        console.print(f"  [dim]Skill dir: {skill_dir.relative_to(project_root)}[/dim]")
        console.print("  Describe what the skill should do. I'll write the implementation.")
        console.print(f"  [dim]Test: /skill test {name}    Exit: /skill dev off[/dim]")
        return

    console.print("[dim]Usage: /skill list | new <name> | test <name> [args] | dev <name> | dev off[/dim]")


def _normalize_skill_name(name: str) -> str:
    """Normalize skill name to snake_case."""
    return name.replace("-", "_")


def _to_slug(name: str) -> str:
    """Convert snake_case skill name to kebab-case directory slug."""
    return _normalize_skill_name(name).replace("_", "-")


def _to_class(name: str) -> str:
    """Convert snake_case or kebab-case skill name to PascalCase."""
    return "".join(w.capitalize() for w in _normalize_skill_name(name).split("_"))


def _build_skill_dev_context(name: str, skill_dir: Path) -> str:
    """Build the skill dev context string injected as project_rules."""
    parts = [
        f"# SKILL DEV MODE: {name}",
        "",
        "You are helping develop a local skill. The user describes what the skill should do.",
        "Write/modify skill.py using the str_replace or write_file tool.",
        f"Skill directory: {skill_dir}",
        "",
        "## Skill Framework Rules",
        "- Inherit `Skill[InputType, OutputType]` from `core.skills.base`",
        "- Input: subclass `SkillInput`, add fields with `Field(description=...)`",
        "- Output: subclass `SkillOutput`, add custom fields",
        "- `execute()` is async, return Output with `success=True/False`",
        "- On error: return `Output(success=False, error='message')` — never raise",
        "- The ToolRouter serializes the full Output as JSON for the LLM",
        "- User tests with `/skill test <name> {json_args}`",
        "",
        "## Current Skill Files",
    ]
    for f in sorted(skill_dir.iterdir()):
        if f.is_file() and f.suffix in (".py", ".md", ".yaml", ".yml"):
            try:
                parts.append(f"\n### {f.name}\n```{f.suffix.lstrip('.')}\n{f.read_text().rstrip()}\n```")
            except OSError:
                parts.append(f"\n### {f.name}\n(unreadable)")
    return "\n".join(parts)


def _validate_skill_output(data: dict) -> list[str]:
    """Check common skill output issues, return warnings."""
    warnings = []
    if data.get("success") and not data.get("error"):
        custom = {k: v for k, v in data.items() if k not in ("success", "result", "error", "cost")}
        if custom and all(_is_empty(v) for v in custom.values()):
            warnings.append("All output fields are empty — skill may not be returning data")
    if not data.get("success") and not data.get("error"):
        warnings.append("success=False but no error message — add error field for debugging")
    return warnings


def _is_empty(v) -> bool:
    if v is None:
        return True
    if isinstance(v, (str, list, dict)) and not v:
        return True
    return False


SLASH_COMMANDS = {
    "/help": cmd_help,
    "/model": cmd_model,
    "/session": cmd_session,
    "/clear": cmd_clear,
    "/verbose": cmd_verbose,
    "/compact": cmd_compact,
    "/history": cmd_history,
    "/copy": cmd_copy,
    "/doctor": cmd_doctor,
    "/version": cmd_version,
    "/login": cmd_login,
    "/logout": cmd_logout,
    "/skill": cmd_skill,
}


# ============================================================================
# Edge turn runner
# ============================================================================

async def _run_edge_turn(user_input, api_client, session_id, model, agent_id, auto_approve, renderer=None, extra_rules=None):
    """Run one edge chat loop turn using the provided APIClient."""
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

    from core.skills.loader import SkillLoader
    builtin_names = {t.name for t in router.list_tools()}
    for local in SkillLoader.discover(SkillLoader.default_paths(project_root)):
        if local.skill.name in builtin_names:
            logger.warning("Local skill '%s' skipped — conflicts with built-in tool", local.skill.name)
            continue
        router.register(local.skill)

    from cli.tools.introspection import GetAgentInfoTool
    session_info = {"session_id": session_id, "agent_id": agent_id, "model": model, "turn": 0}
    perms = PermissionManager(auto_approve=auto_approve)

    router.register(GetAgentInfoTool(tool_router=router, session_info=session_info, api_client=api_client))
    return await edge_chat_loop(
        user_input, api_client, router, perms,
        session_id=session_id, project_root=project_root,
        model=model, agent_id=agent_id,
        session_info=session_info,
        renderer=renderer,
        extra_rules=extra_rules,
    )


# ============================================================================
# Click CLI
# ============================================================================

@click.group()
@click.option("--api-url", default="http://localhost:8000", envvar="MO_AGENT_API_URL")
@click.option("--profile", default=None, envvar="MO_AGENT_PROFILE", help="Profile to use")
@click.pass_context
@click.version_option(version=VERSION)
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
        if "Username already exists" in error_msg:
            click.echo(f"❌ Username '{username}' is already taken")
        elif "Email already exists" in error_msg:
            click.echo(f"❌ Email '{email}' is already registered")
        elif "String should have at least 8 characters" in error_msg and "password" in error_msg:
            click.echo("❌ Password must be at least 8 characters")
        elif "422" in error_msg:
            click.echo("❌ Invalid input")
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


@cli.command()
@click.option("--user-id", default="cli_user")
@click.option("--session-id", default=None)
@click.option("--model", default=None, help="Model to use for chat")
@click.option("--resume", is_flag=True, help="Resume last session")
@click.option("--auto-approve", is_flag=True, help="Auto-approve tool execution")
@click.option("--debug", is_flag=True, help="Print full traceback on errors")
@click.pass_context
def chat(ctx, user_id, session_id, model, resume, auto_approve, debug):
    """Start interactive chat with edge tool execution."""
    from rich.console import Console
    from rich.panel import Panel

    client = ctx.obj["client"]
    console = Console(stderr=True)
    is_tty = sys.stdin.isatty()

    if debug:
        from rich.traceback import install
        install(show_locals=True)

    # --- Auth ---
    auth_result = client.ensure_authenticated()
    if auth_result == "session_expired" or not auth_result:
        if not is_tty:
            console.print("[red]Not logged in[/red]")
            sys.exit(1)
        console.print()
        console.print("[yellow]⚠  Not logged in[/yellow]")
        console.print()
        console.print("  [green]1[/green]  Login")
        console.print("  [cyan]2[/cyan]  Register")
        console.print("  [dim]3[/dim]  Exit")
        console.print()
        choice = console.input("[dim]Choose [1/2/3]:[/dim] ").strip()
        choice = int(choice) if choice in ("1", "2", "3") else 1
        if choice == 3:
            sys.exit(0)
        console.print()
        username = console.input("[bold]Username:[/bold] ").strip()
        password = click.prompt("Password", hide_input=True)
        if choice == 2:
            email = console.input("[bold]Email:[/bold] ").strip()
            try:
                client.register(username, password, email)
            except Exception as e:
                console.print(f"\n[red]✗ Registration failed:[/red] {e}")
                sys.exit(1)
        try:
            client.login(username, password)
            console.print(f"[green]✓[/green] Logged in as [bold]{username}[/bold]")
        except Exception as e:
            console.print(f"\n[red]✗ Login failed:[/red] {e}")
            sys.exit(1)
        console.print()

    try:
        user_info = client.get_current_user()
        username = user_info.get("username", "You")
    except Exception:
        username = "You"

    selected_model = model
    try:
        _profile = APIClient.load_profile(profile=client.profile)
        if not selected_model and _profile.get("default_model"):
            selected_model = _profile["default_model"]
        if resume and not session_id and _profile.get("last_session_id"):
            session_id = _profile["last_session_id"]
    except Exception:
        pass

    # --- Session ---
    try:
        if not session_id:
            result = client.create_session(agent_id=user_id or "default-agent")
            session_id = result["session_id"]
        try:
            client.save_profile_setting(last_session_id=session_id)
        except Exception:
            pass
    except Exception as e:
        console.print(f"[red]Failed to create session: {e}[/red]")
        sys.exit(1)

    # --- Welcome banner ---
    console.print(Panel(
        f"[cyan bold]mo-agent[/cyan bold] v{VERSION}\n"
        f"📝 Session: {session_id}\n"
        f"🤖 Model: {selected_model or '(default)'}",
        border_style="bright_black", title="✦ mo-agent", title_align="left",
    ))
    console.print("Type [cyan]/help[/cyan] for commands, [cyan]/exit[/cyan] to quit\n", style="dim")

    # --- State shared with slash commands ---
    state = {
        "session_id": session_id,
        "selected_model": selected_model,
        "last_response": "",
        "turn_history": [],
    }

    # --- Choose renderer + input method ---
    if is_tty:
        from cli.ui.renderer import RichRenderer
        from cli.ui.repl import create_session as create_repl_session, get_input
        from cli.ui.status_bar import StatusBar

        status_bar = StatusBar()
        status_bar.update(session_id=session_id, model=selected_model or "")
        repl_session = create_repl_session(bottom_toolbar=status_bar.toolbar)
        renderer = RichRenderer(console=console)
    else:
        from cli.ui.renderer import SimpleRenderer
        renderer = SimpleRenderer()
        status_bar = None
        repl_session = None

    try:
        turn_count = 0
        while True:
            # --- Input ---
            if is_tty:
                inp = get_input(repl_session, "❯ ")
                if inp.eof:
                    break
                if inp.interrupted:
                    continue
                user_input = inp.text.strip()
            else:
                try:
                    user_input = input().strip()
                except EOFError:
                    break

            if not user_input:
                continue

            # --- Slash commands ---
            if user_input.startswith("/"):
                parts = user_input.split(maxsplit=1)
                cmd_name = parts[0].lower()
                cmd_arg = parts[1] if len(parts) > 1 else None

                if cmd_name in ("/exit", "/quit"):
                    break

                handler = SLASH_COMMANDS.get(cmd_name)
                if handler:
                    handler(
                        console=console, client=client, cmd_arg=cmd_arg,
                        session_id=state["session_id"], username=username,
                        user_id=user_id, state=state, status_bar=status_bar,
                        selected_model=state.get("selected_model"),
                    )
                    # Sync state changes back
                    session_id = state["session_id"]
                    selected_model = state.get("selected_model")
                    if status_bar:
                        status_bar.update(session_id=session_id, model=selected_model or "")
                else:
                    console.print(f"[red]Unknown command.[/red] Type [cyan]/help[/cyan]")
                continue

            if user_input.lower() in ("exit", "quit"):
                break

            # --- Chat turn ---
            try:
                # In skill dev mode, refresh context from disk each turn so the
                # LLM always sees the latest file contents after edits.
                skill_dev_rules = None
                if state.get("skill_dev_name"):
                    skill_dir = Path(state.get("skill_dev_dir", ""))
                    if skill_dir.is_dir():
                        state["skill_dev_context"] = _build_skill_dev_context(
                            state["skill_dev_name"], skill_dir,
                        )
                    skill_dev_rules = state.get("skill_dev_context")

                if hasattr(renderer, "begin_response"):
                    renderer.begin_response()
                result_text = client._run(_run_edge_turn(
                    user_input, client._ensure_client(), state["session_id"],
                    state.get("selected_model"), user_id, auto_approve,
                    renderer=renderer,
                    extra_rules=skill_dev_rules,
                ))
                if hasattr(renderer, "end_response"):
                    renderer.end_response()
                turn_count += 1
                state["last_response"] = result_text or ""
                state["turn_history"].append({"role": "user", "preview": user_input[:80]})
                state["turn_history"].append({"role": "assistant", "preview": (result_text or "")[:80]})
                if status_bar:
                    status_bar.update(turn=turn_count)
            except AuthenticationError:
                if hasattr(renderer, "end_response"):
                    renderer.end_response()
                if not is_tty:
                    console.print("[red]Session expired[/red]")
                    break
                console.print("\n[yellow]⚠  Session expired — re-login required[/yellow]\n")
                try:
                    _username = console.input("[bold]Username:[/bold] ").strip()
                    _password = click.prompt("Password", hide_input=True)
                    client.login(_username, _password)
                    console.print(f"[green]✓[/green] Logged in as [bold]{_username}[/bold]\n")
                    console.print("[dim]Re-send your message.[/dim]")
                except (EOFError, KeyboardInterrupt):
                    console.print("\n[dim]Cancelled[/dim]")
                except Exception as e:
                    console.print(f"[red]✗ Login failed:[/red] {e}")
                    break
            except KeyboardInterrupt:
                if hasattr(renderer, "end_response"):
                    renderer.end_response()
                console.print("\n[dim]Interrupted[/dim]")
            except Exception as e:
                if hasattr(renderer, "end_response"):
                    renderer.end_response()
                if debug:
                    console.print_exception(show_locals=True)
                else:
                    console.print(f"[red]{type(e).__name__}: {e}[/red]")
            console.print()

    except KeyboardInterrupt:
        console.print("\n[dim]Interrupted[/dim]")
    finally:
        try:
            client.close_session(state["session_id"])
            console.print("[green]✓[/green] Session closed")
        except Exception:
            pass


@cli.command()
@click.pass_context
def doctor(ctx):
    """Run diagnostics."""
    from rich.console import Console
    from cli.ui.doctor import run_doctor
    run_doctor(Console(stderr=True), ctx.obj["client"])


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


@skill.command("scaffold")
@click.argument("yaml_file", type=click.Path(exists=True))
@click.option("--output-dir", default="skills/", type=click.Path(), help="Output directory")
def skill_scaffold(yaml_file, output_dir):
    """Generate skill package from YAML declaration."""
    import yaml as _yaml
    from core.skills.scaffold import SkillSpec, generate_files
    try:
        data = _yaml.safe_load(Path(yaml_file).read_text())
        spec = SkillSpec.from_dict(data)
        target = Path(output_dir) / spec.name
        if target.exists():
            click.echo(f"❌ Directory already exists: {target}")
            return
        files = generate_files(spec)
        target.mkdir(parents=True)
        for fname, content in files.items():
            (target / fname).write_text(content)
        click.echo(f"✅ Generated skill package: {target}/")
        for fname in files:
            click.echo(f"   {fname}")
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
