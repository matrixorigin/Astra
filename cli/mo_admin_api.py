#!/usr/bin/env python3
"""mo-admin CLI — administrative interface for mo-agent-engine.

Reuses SyncAPIClient from mo-agent for consistent auth/session handling.
"""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent.parent))

import json
from datetime import datetime, timedelta

import click

from cli.mo_agent_api import SyncAPIClient

VERSION = "0.1.0"


def require_auth(client: SyncAPIClient) -> None:
    result = client.ensure_authenticated()
    if result == "session_expired":
        click.echo("❌ Session expired — please login again: mo-admin login")
        sys.exit(1)
    if not result:
        click.echo("❌ Please login first: mo-admin login")
        sys.exit(1)


# ============================================================================
# Root group
# ============================================================================

@click.group()
@click.option("--api-url", default="http://localhost:8000", envvar="MO_AGENT_API_URL")
@click.option("--profile", default=None, envvar="MO_AGENT_PROFILE", help="Profile to use")
@click.pass_context
@click.version_option(version=VERSION)
def cli(ctx, api_url, profile):
    """mo-admin — Administrative interface for mo-agent-engine."""
    ctx.ensure_object(dict)
    ctx.obj["client"] = SyncAPIClient(api_url, profile=profile)


# ============================================================================
# Auth
# ============================================================================

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


@cli.command()
@click.option("--username", prompt=True)
@click.option("--password", prompt=True, hide_input=True)
@click.option("--email", default=None)
@click.pass_context
def register(ctx, username, password, email):
    """Register new admin user."""
    try:
        ctx.obj["client"].admin_register(
            username=username, password=password,
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
        r = client.get_current_user()
        for key in ("username", "email", "user_id", "role"):
            if key in r:
                click.echo(f"{key.replace('_', ' ').title()}: {r[key]}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")
        sys.exit(1)


@cli.command()
@click.pass_context
def init(ctx):
    """Initialize database."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        result = client.admin_init()
        click.echo(f"✅ Database initialized — tables created: {result.get('tables_created', 0)}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


# ============================================================================
# Model management
# ============================================================================

@cli.group()
def model():
    """Manage LLM models."""


@model.command("add")
@click.argument("model_name")
@click.argument("provider")
@click.option("--api-key", prompt="API key", hide_input=True, help="LLM provider API key")
@click.option("--base-url", default=None, help="Custom base URL (OpenAI-compatible)")
@click.pass_context
def model_add(ctx, model_name, provider, api_key, base_url):
    """Register a model with API key. Validates connectivity."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        result = client.admin_create_model(
            model_name=model_name, provider=provider,
            api_key=api_key, base_url=base_url,
        )
        conn = result.get("connectivity", "unknown")
        if result.get("is_active"):
            click.echo(f"✅ Model registered: {model_name} ({provider}) — connectivity: {conn}")
        else:
            click.echo(f"⚠️  Model registered as INACTIVE: {model_name} ({provider})")
            click.echo(f"   Reason: {conn}")
            click.echo(f"   Fix: mo-admin model update {model_name} --api-key <key>")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@model.command("list")
@click.pass_context
def model_list(ctx):
    """List registered models."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        models = client.admin_list_models()
        if not models:
            click.echo("No models registered")
            click.echo("\nRegister one with:")
            click.echo("  mo-admin model add <name> <provider> --api-key <key>")
            return
        click.echo("Models:")
        for m in models:
            status = "✓" if m.get("is_active") else "✗"
            click.echo(f"  {status} {m['name']} ({m['provider']})")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@model.command("show")
@click.argument("model_name")
@click.pass_context
def model_show(ctx, model_name):
    """Show model details."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        # TODO: use admin_get_model(name) when API adds a single-model endpoint
        models = client.admin_list_models()
        m = next((m for m in models if m["name"] == model_name), None)
        if not m:
            click.echo(f"❌ Model '{model_name}' not found")
            sys.exit(1)
        for k, v in m.items():
            if k not in ("api_key",):
                click.echo(f"  {k}: {v}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@model.command("update")
@click.argument("model_name")
@click.option("--api-key", default=None, hide_input=True, help="New API key")
@click.option("--base-url", default=None, help="New base URL")
@click.option("--activate/--deactivate", default=None, help="Set model active/inactive")
@click.pass_context
def model_update(ctx, model_name, api_key, base_url, activate):
    """Update model config or API key."""
    client = ctx.obj["client"]
    require_auth(client)
    if api_key is None and base_url is None and activate is None:
        click.echo("Nothing to update. Use --api-key, --base-url, --activate, or --deactivate")
        return
    try:
        result = client.admin_update_model(
            model_name=model_name, api_key=api_key,
            base_url=base_url, is_active=activate,
        )
        conn = result.get("connectivity")
        if conn and conn != "ok":
            click.echo(f"⚠️  Updated but connectivity failed: {conn}")
        else:
            click.echo(f"✅ Model updated: {model_name}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@model.command("check")
@click.argument("model_name")
@click.pass_context
def model_check(ctx, model_name):
    """Re-check model connectivity."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        result = client.admin_check_model(model_name)
        conn = result.get("connectivity", "unknown")
        status = "✅" if result.get("is_active") else "❌"
        click.echo(f"{status} {model_name}: {conn}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")


@model.command("remove")
@click.argument("model_name")
@click.option("--yes", "-y", is_flag=True, help="Skip confirmation")
@click.pass_context
def model_remove(ctx, model_name, yes):
    """Remove a model."""
    client = ctx.obj["client"]
    require_auth(client)
    if not yes:
        click.confirm(f"Remove model '{model_name}'?", abort=True)
    try:
        client.admin_delete_model(model_name)
        click.echo(f"✅ Model removed: {model_name}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


# ============================================================================
# Token management
# ============================================================================

@cli.group()
def token():
    """Manage API tokens."""


@token.command("create")
@click.option("--type", "token_type", type=click.Choice(["llm", "github", "api"]), required=True)
@click.option("--provider", required=True)
@click.option("--value", prompt=True, hide_input=True)
@click.option("--scope", "scope_type", default="global")
@click.option("--scope-id", default=None)
@click.pass_context
def token_create(ctx, token_type, provider, value, scope_type, scope_id):
    """Create API token."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        result = client.admin_create_token(
            token_type=token_type, provider=provider,
            token_value=value, scope=scope_type, scope_id=scope_id,
        )
        click.echo(f"✅ Token created: {result['token_id']}")
    except Exception as e:
        click.echo(f"❌ Failed: {e}")
        sys.exit(1)


@token.command("list")
@click.option("--type", "token_type", default=None)
@click.pass_context
def token_list(ctx, token_type):
    """List tokens."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        tokens = client.admin_list_tokens(token_type=token_type)
        if not tokens:
            click.echo("No tokens found")
            return
        click.echo("API Tokens:")
        click.echo("=" * 80)
        for t in tokens:
            status = "🟢" if t.get("is_active") else "⚪"
            click.echo(
                f"{status} {t['token_id'][:8]}... | "
                f"{t.get('token_type', '?'):8} | "
                f"{t.get('provider', '?'):15} | "
                f"{t.get('scope', '?'):10}"
            )
    except Exception as e:
        click.echo(f"❌ Error: {e}")


# ============================================================================
# User management
# ============================================================================

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


# ============================================================================
# Skill management (NEW — admin-side skill lifecycle)
# ============================================================================

@cli.group()
def skill():
    """Manage skills (admin operations)."""


@skill.command("list")
@click.pass_context
def skill_list(ctx):
    """List registered skills."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        skills = client.list_skills()
        if not skills:
            click.echo("No skills found")
            return
        click.echo("Skills:")
        click.echo("=" * 70)
        for s in skills:
            status = "✓" if s.get("is_active") else "✗"
            st = s.get("status", "active")
            click.echo(f"  {status} {s['skill_name']} v{s['version']}  [{st}]")
            if s.get("description"):
                click.echo(f"    {s['description'][:60]}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@skill.command("show")
@click.argument("skill_name")
@click.pass_context
def skill_show(ctx, skill_name):
    """Show skill details."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        s = client.get_skill(skill_name)
        for k in ("skill_name", "version", "status", "source", "description",
                   "is_active", "is_public", "created_by", "created_at"):
            if k in s:
                click.echo(f"  {k}: {s[k]}")
        if s.get("manifest"):
            deps = s["manifest"].get("depends_on", [])
            if deps:
                click.echo(f"  depends_on: {json.dumps(deps)}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@skill.command("versions")
@click.argument("skill_name")
@click.pass_context
def skill_versions(ctx, skill_name):
    """List all versions of a skill."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        versions = client.get_skill_versions(skill_name)
        if not versions:
            click.echo(f"No versions found for '{skill_name}'")
            return
        for v in versions:
            active = "→" if v.get("is_active") else " "
            click.echo(
                f"  {active} v{v['version']}  [{v.get('status', '?')}]  "
                f"{v.get('created_at', '')}"
            )
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@skill.command("register")
@click.argument("skill_file", type=click.Path(exists=True))
@click.pass_context
def skill_register(ctx, skill_file):
    """Register skill from JSON file."""
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


@skill.command("upgrade-check")
@click.argument("skill_name")
@click.argument("new_version")
@click.pass_context
def skill_upgrade_check(ctx, skill_name, new_version):
    """Check what breaks when upgrading a skill to a new version."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        skills = client.list_skills()
        if not skills:
            click.echo("No skills found")
            return
        from core.skills.resolver import DependencyResolver
        available = {
            s["skill_name"]: {
                "version": s.get("version", "0.0.0"),
                "depends_on": (s.get("manifest") or {}).get("depends_on", []),
            }
            for s in skills
        }
        resolver = DependencyResolver(available_skills=available)
        broken = resolver.analyze_upgrade_impact(skill_name, new_version)
        if not broken:
            click.echo(f"✅ Upgrading {skill_name} to {new_version} breaks nothing")
        else:
            click.echo(f"⚠️  Upgrading {skill_name} to {new_version} would break:")
            for dep_name, constraint in broken:
                click.echo(f"  • {dep_name} (requires {constraint})")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


# ============================================================================
# Prompt management (NEW)
# ============================================================================

@cli.group()
def prompt():
    """Manage prompts."""


@prompt.command("optimize")
@click.argument("prompt_text")
@click.option("--model", default=None, help="Model to optimize for")
@click.pass_context
def prompt_optimize(ctx, prompt_text, model):
    """Optimize a prompt using the platform's prompt optimizer."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        result = client.admin_optimize_prompt(prompt_text=prompt_text, model=model)
        click.echo("Optimized prompt:")
        click.echo("-" * 40)
        click.echo(result.get("optimized", result.get("prompt", prompt_text)))
        if result.get("changes"):
            click.echo(f"\nChanges: {result['changes']}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


# ============================================================================
# Audit
# ============================================================================

@cli.group()
def audit():
    """View audit logs."""


@audit.command("logs")
@click.option("--user", default=None)
@click.option("--since", default=None, help="Date filter (YYYY-MM-DD)")
@click.option("--limit", default=50)
@click.pass_context
def audit_logs(ctx, user, since, limit):
    """View audit logs."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        since_date = datetime.strptime(since, "%Y-%m-%d").isoformat() if since else None
        logs = client.admin_auth_audit_logs(user_id=user, since=since_date, limit=limit)
        if not logs:
            click.echo("No logs found")
            return
        click.echo("Audit Logs:")
        click.echo("=" * 80)
        for log in logs:
            click.echo(f"[{log.get('created_at')}] {log.get('action')} by {log.get('user_id')}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


# ============================================================================
# Feedback
# ============================================================================

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
        click.echo("=" * 40)
        click.echo(f"  Total:    {stats.get('total', 0)}")
        click.echo(f"  Positive: {stats.get('positive', 0)}")
        click.echo(f"  Negative: {stats.get('negative', 0)}")
        click.echo(f"  Average:  {stats.get('avg_rating', 0):.2f}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


@feedback.command("export")
@click.pass_context
def feedback_export(ctx):
    """Export feedback (async job)."""
    client = ctx.obj["client"]
    require_auth(client)
    try:
        result = client.admin_feedback_export()
        if result.get("download_url"):
            click.echo(f"✅ Export ready: {result['download_url']}")
        else:
            click.echo(f"📋 Export job created: {result.get('job_id')}")
            click.echo(f"   Status: {result.get('status')}")
    except Exception as e:
        click.echo(f"❌ Error: {e}")


# ============================================================================
# Interactive shell
# ============================================================================

@cli.command()
@click.pass_context
def shell(ctx):
    """Enter interactive admin shell (REPL)."""
    try:
        from prompt_toolkit import PromptSession
        from prompt_toolkit.history import InMemoryHistory
        from prompt_toolkit.auto_suggest import AutoSuggestFromHistory
    except ImportError:
        click.echo("❌ prompt_toolkit required: pip install prompt_toolkit")
        sys.exit(1)

    client = ctx.obj["client"]
    require_auth(client)

    # Collect all leaf command paths for completion
    cmd_names = _collect_commands(cli)

    try:
        from prompt_toolkit.completion import WordCompleter
        completer = WordCompleter(cmd_names, sentence=True)
    except ImportError:
        completer = None

    click.echo(f"mo-admin {VERSION} — interactive shell")
    click.echo("Type commands without 'mo-admin' prefix. 'help' for commands, 'exit' to quit.\n")

    session = PromptSession(
        history=InMemoryHistory(),
        auto_suggest=AutoSuggestFromHistory(),
        completer=completer,
    )

    while True:
        try:
            line = session.prompt("mo-admin> ").strip()
        except (EOFError, KeyboardInterrupt):
            break
        if not line:
            continue
        if line in ("exit", "quit"):
            break
        if line == "help":
            # Re-invoke the CLI help
            try:
                cli(["--help"], standalone_mode=False, obj=ctx.obj)
            except SystemExit:
                pass
            continue

        # Parse and invoke as if it were a CLI command
        args = line.split()
        try:
            cli(args, standalone_mode=False, obj=ctx.obj)
        except SystemExit:
            pass  # Click raises SystemExit on --help or errors
        except click.exceptions.UsageError as e:
            click.echo(f"❌ {e}")
        except Exception as e:
            click.echo(f"❌ {e}")

    click.echo("Bye.")


def _collect_commands(group, prefix=""):
    """Recursively collect all command paths for completion."""
    names = []
    for name, cmd in group.commands.items():
        full = f"{prefix} {name}".lstrip()
        names.append(full)
        if isinstance(cmd, click.Group):
            names.extend(_collect_commands(cmd, full))
    return names


if __name__ == "__main__":
    cli()
