#!/usr/bin/env python3
"""mo-agent CLI - Command-line interface for mo-dev-agent."""

import click
import sys
from pathlib import Path

# Add project root to path
sys.path.insert(0, str(Path(__file__).parent.parent))

from sdk import Database
from core.events.session_manager import SessionManager
from core.events.event_logger import EventLogger
from core.llm.client import LLMClient
from core.context import ContextManager, TaskType
from core.skills.registry import SkillRegistry
from core.skills.builtin import register_builtin_skills
from core.agent.selector import AgentSkillSelector
from core.agent.executor import AgentExecutor
from core.agent.chat_loop import ChatLoop
from core.skills.mocking import MockMode
from core.events.models import StreamEventType
from core.llm.router import ModelConfig
from core.llm.models import LLMProvider
import asyncio
import json


@click.group()
@click.version_option(version="0.1.0")
def cli():
    """mo-agent - Event-centric intelligent agent platform."""
    pass


@cli.command()
@click.option('--user-id', default='cli_user', help='User identifier')
@click.option('--model', default='gpt-4', help='LLM model to use')
@click.option('--mode', type=click.Choice(['production', 'replay']), default='production', help='Execution mode')
def chat(user_id, model, mode):
    """Start interactive chat session."""
    click.echo(f"🤖 mo-agent interactive chat (Mode: {mode})")
    click.echo("=" * 50)
    
    # Initialize
    db = Database()
    session_mgr = SessionManager(db)
    logger = EventLogger(db)
    llm_client = LLMClient(db)
    context_mgr = ContextManager(db)
    
    # Agent Components
    selector = AgentSkillSelector(db, llm_client)
    executor = AgentExecutor(
        db=db, 
        registry=None,  # Will be set after skill registration
        mode=MockMode(mode)
    )
    chat_loop = ChatLoop(
        selector=selector, 
        executor=executor, 
        llm_client=llm_client,
        event_logger=logger
    )
    
    # Create agent registry with example agents
    from core.agent.agent_registry import AgentRegistry, AgentProfile
    agent_registry = AgentRegistry()
    
    # Register example agents
    agent_registry.register(AgentProfile(
        agent_id="code_reviewer",
        system_prompt="You are a code review expert. Analyze code for bugs, style issues, and performance problems. Provide constructive feedback.",
        skill_filter=["summarize_pr", "analyze_code"]
    ))
    agent_registry.register(AgentProfile(
        agent_id="security_auditor",
        system_prompt="You are a security auditor. Review code for security vulnerabilities, input validation issues, and potential exploits.",
        skill_filter=["summarize_pr", "analyze_code"]
    ))
    agent_registry.register(AgentProfile(
        agent_id="documentation_writer",
        system_prompt="You are a documentation writer. Create clear, comprehensive documentation for code, APIs, and features.",
        skill_filter=["summarize_pr", "write_docs"]
    ))
    
    # Create chat_loop_factory for delegation
    def create_chat_loop(system_prompt=None):
        return ChatLoop(
            selector=AgentSkillSelector(db, llm_client),
            executor=AgentExecutor(
                db=db,
                registry=SkillRegistry(db),
                mode=MockMode(mode)
            ),
            llm_client=llm_client,
            event_logger=EventLogger(db)
        )
    
    # Register skills with agent registry
    skill_registry = SkillRegistry(db)
    register_builtin_skills(
        skill_registry,
        db,
        agent_registry=agent_registry,
        chat_loop_factory=create_chat_loop
    )
    
    # Update executor with registered skills
    executor.registry = skill_registry

    # Create session
    session = session_mgr.create_session(user_id=user_id)
    click.echo(f"Session: {session.session_id}")
    click.echo("Type 'exit' or 'quit' to end session\n")
    
    # Set user context for LLM client (for scope-based access control)
    llm_client.set_user_context(user_id=user_id)
    
    try:
        while True:
            # Get user input
            user_input = click.prompt('You', type=str, prompt_suffix='> ')
            
            if user_input.lower() in ['exit', 'quit']:
                break
            
            # Run Chat Loop Step
            click.echo("Agent> ", nl=False)
            
            # Build context from history
            try:
                ctx = context_mgr.build_context(
                    session_id=session.session_id,
                    query=user_input,
                )
                context_dict = {
                    "system_prompt": ctx.system_prompt,
                    "selected_events": ctx.selected_events,
                }
            except Exception:
                context_dict = None
            
            # Run async loop with streaming
            async def _stream_response():
                async for event in chat_loop.run_step_stream(
                    user_input=user_input,
                    session_id=session.session_id,
                    user_id=user_id,
                    context=context_dict,
                ):
                    if event.event_type == StreamEventType.TEXT_DELTA:
                        click.echo(event.data.get("chunk", ""), nl=False)
                    elif event.event_type == StreamEventType.THINKING_DELTA:
                        click.echo(f"\n  🤔 {event.data.get('chunk', '')}", nl=False)
                    elif event.event_type == StreamEventType.TOOL_CALL_START:
                        click.echo(f"\n  🔧 {event.data.get('tool', 'unknown')}...", nl=False)
                    elif event.event_type == StreamEventType.TOOL_RESULT:
                        click.echo(" ✓", nl=False)
                    elif event.event_type == StreamEventType.RUN_FINISHED:
                        click.echo()  # Final newline
            
            asyncio.run(_stream_response())
            click.echo()
            
    except KeyboardInterrupt:
        click.echo("\n\nSession interrupted")
    except Exception as e:
        click.echo(f"\nError: {e}")
    finally:
        # Close session
        session_mgr.close_session(session.session_id)
        click.echo(f"\n✅ Session closed: {session.session_id}")


@cli.group()
def skill():
    """Manage skills."""
    pass


@skill.command('list')
@click.option('--active-only', is_flag=True, help='Show only active skills')
def skill_list(active_only):
    """List available skills."""
    db = Database()
    
    # Query skills directly
    query = "SELECT * FROM skills_registry"
    if active_only:
        query += " WHERE is_active = TRUE"
    query += " ORDER BY skill_name, version DESC"
    
    skills = db.fetchall(query)
    
    if not skills:
        click.echo("No skills registered")
        return
    
    click.echo("Available Skills:")
    click.echo("=" * 70)
    
    for skill in skills:
        status = "✓" if skill.get('is_active', True) else "✗"
        click.echo(f"{status} {skill['skill_name']} v{skill['version']}")
        click.echo(f"  {skill['description']}")
        click.echo()


@skill.command('register')
@click.argument('skill_file', type=click.Path(exists=True))
def skill_register(skill_file):
    """Register a new skill from file."""
    import json
    
    db = Database()
    registry = SkillRegistry(db)
    
    with open(skill_file) as f:
        skill_data = json.load(f)
    
    skill_id = registry.register(
        name=skill_data['name'],
        version=skill_data['version'],
        description=skill_data['description'],
        input_schema=skill_data['input_schema'],
        output_schema=skill_data['output_schema'],
        implementation=skill_data['implementation']
    )
    
    click.echo(f"✅ Skill registered: {skill_data['name']} v{skill_data['version']}")
    click.echo(f"   Skill ID: {skill_id}")


@cli.group()
def model():
    """Manage LLM models."""
    pass


@model.command('list')
@click.option('--user-id', help='Filter by user scope')
@click.option('--tenant-id', help='Filter by tenant scope')
def model_list(user_id, tenant_id):
    """List available models."""
    db = Database()
    
    # Get models from router
    client = LLMClient(db=db, user_id=user_id, tenant_id=tenant_id)
    models = client.router.list_models()
    
    if not models:
        click.echo("No models available")
        return
    
    click.echo("Available Models:")
    click.echo("=" * 80)
    
    for m in models:
        status = "✓" if m.is_active else "✗"
        click.echo(f"{status} {m.model_name}")
        click.echo(f"   Provider: {m.provider.value}")
        click.echo(f"   Context: {m.context_window:,} tokens")
        click.echo(f"   Price: ${m.price_per_1k_prompt:.4f}/1K prompt + ${m.price_per_1k_completion:.4f}/1K completion")
        if m.fallback_to:
            click.echo(f"   Fallback: {m.fallback_to}")
        click.echo()


@model.command('add')
@click.argument('model_name')
@click.argument('provider', type=click.Choice([p.value for p in LLMProvider]))
@click.option('--context-window', default=128000, help='Context window size')
@click.option('--price-prompt', default=0.01, help='Price per 1K prompt tokens')
@click.option('--price-completion', default=0.03, help='Price per 1K completion tokens')
@click.option('--rpm-limit', default=500, help='Requests per minute limit')
@click.option('--tpm-limit', default=150000, help='Tokens per minute limit')
@click.option('--scope', type=click.Choice(['global', 'user', 'tenant']), default='global', help='Scope for model')
@click.option('--scope-id', help='User or tenant ID for scope')
@click.option('--fallback', help='Fallback model name')
@click.option('--tags', help='Comma-separated tags (e.g., fast,cheap,reasoning)')
def model_add(model_name, provider, context_window, price_prompt, price_completion, 
              rpm_limit, tpm_limit, scope, scope_id, fallback, tags):
    """Add a new model to the registry."""
    db = Database()
    
    # Validate scope_id
    if scope in ['user', 'tenant'] and not scope_id:
        click.echo(f"❌ --scope-id is required for {scope} scope")
        return
    
    # Validate model_name format
    if not model_name or len(model_name) < 3:
        click.echo(f"❌ Invalid model name: must be at least 3 characters")
        return
    
    # Validate prices
    if price_prompt < 0 or price_completion < 0:
        click.echo(f"❌ Prices must be non-negative")
        return
    
    # Parse tags
    tag_list = [t.strip() for t in tags.split(',')] if tags else []
    
    # Create model config
    new_config = ModelConfig(
        model_name=model_name,
        provider=LLMProvider(provider),
        context_window=context_window,
        price_per_1k_prompt=price_prompt,
        price_per_1k_completion=price_completion,
        rpm_limit=rpm_limit,
        tpm_limit=tpm_limit,
        fallback_to=fallback,
        tags=tag_list,
    )
    
    try:
        # Read existing registry
        query = "SELECT value FROM configs WHERE key_name = 'model_registry'"
        if scope == 'global':
            query += " AND scope_type = 'global'"
        elif scope == 'user':
            query += f" AND scope_type = 'user' AND scope_id = '{scope_id}'"
        elif scope == 'tenant':
            query += f" AND scope_type = 'tenant' AND scope_id = '{scope_id}'"
        
        row = db.fetchone(query)
        
        if row:
            # Merge with existing
            existing = json.loads(row['value'])
            # Check if model already exists
            existing_names = [m['model_name'] for m in existing]
            if model_name in existing_names:
                click.echo(f"⚠️  Model '{model_name}' already exists, updating...")
                existing = [m for m in existing if m['model_name'] != model_name]
            existing.append(new_config.model_dump())
            registry_value = json.dumps(existing)
        else:
            # Create new registry
            registry_value = json.dumps([new_config.model_dump()])
        
        # Upsert
        config_id = f"model_registry_{scope}" + (f"_{scope_id}" if scope_id else "")
        db.execute(
            """
            INSERT INTO configs (config_id, key_name, value, scope_type, scope_id)
            VALUES (%s, 'model_registry', %s, %s, %s)
            ON DUPLICATE KEY UPDATE value = %s
            """,
            (config_id, registry_value, scope, scope_id, registry_value)
        )
        
        click.echo(f"✅ Model '{model_name}' added successfully")
        click.echo(f"   Provider: {provider}")
        click.echo(f"   Scope: {scope}" + (f" ({scope_id})" if scope_id else ""))
    except Exception as e:
        click.echo(f"❌ Failed to add model: {e}")


@model.command('remove')
@click.argument('model_name')
@click.option('--scope', type=click.Choice(['global', 'user', 'tenant']), default='global')
@click.option('--scope-id', help='User or tenant ID for scope')
@click.option('--force', is_flag=True, help='Force removal without confirmation')
def model_remove(model_name, scope, scope_id, force):
    """Remove a model from the registry."""
    db = Database()
    
    # Validate scope_id
    if scope in ['user', 'tenant'] and not scope_id:
        click.echo(f"❌ --scope-id is required for {scope} scope")
        return
    
    try:
        # Read existing registry
        query = "SELECT value FROM configs WHERE key_name = 'model_registry'"
        if scope == 'global':
            query += " AND scope_type = 'global'"
        elif scope == 'user':
            query += f" AND scope_type = 'user' AND scope_id = '{scope_id}'"
        elif scope == 'tenant':
            query += f" AND scope_type = 'tenant' AND scope_id = '{scope_id}'"
        
        row = db.fetchone(query)
        
        if not row:
            click.echo(f"❌ No model registry found for {scope}" + (f" ({scope_id})" if scope_id else ""))
            return
        
        # Remove model
        existing = json.loads(row['value'])
        existing_names = [m['model_name'] for m in existing]
        
        if model_name not in existing_names:
            click.echo(f"❌ Model '{model_name}' not found in registry")
            click.echo(f"   Available models: {', '.join(existing_names)}")
            return
        
        # Check if other models depend on this one (as fallback)
        dependent_models = [m['model_name'] for m in existing 
                           if m.get('fallback_to') == model_name]
        if dependent_models and not force:
            click.echo(f"⚠️  Warning: The following models use '{model_name}' as fallback:")
            for dep in dependent_models:
                click.echo(f"   - {dep}")
            click.echo(f"\nUse --force to remove anyway")
            return
        
        # Confirm removal
        if not force:
            if not click.confirm(f"Remove model '{model_name}' from {scope} scope?"):
                click.echo("Cancelled")
                return
        
        updated = [m for m in existing if m['model_name'] != model_name]
        
        if not updated:
            # Delete entire config if no models left
            if scope_id:
                db.execute(
                    "DELETE FROM configs WHERE key_name = 'model_registry' AND scope_type = %s AND scope_id = %s",
                    (scope, scope_id)
                )
            else:
                db.execute(
                    "DELETE FROM configs WHERE key_name = 'model_registry' AND scope_type = %s AND scope_id IS NULL",
                    (scope,)
                )
            click.echo(f"✅ Model '{model_name}' removed (registry now empty)")
        else:
            # Update with remaining models
            if scope_id:
                db.execute(
                    "UPDATE configs SET value = %s WHERE key_name = 'model_registry' AND scope_type = %s AND scope_id = %s",
                    (json.dumps(updated), scope, scope_id)
                )
            else:
                db.execute(
                    "UPDATE configs SET value = %s WHERE key_name = 'model_registry' AND scope_type = %s AND scope_id IS NULL",
                    (json.dumps(updated), scope)
                )
            click.echo(f"✅ Model '{model_name}' removed successfully")
            click.echo(f"   Remaining models: {len(updated)}")
    except Exception as e:
        click.echo(f"❌ Failed to remove model: {e}")


@model.command('show')
@click.argument('model_name')
@click.option('--user-id', help='User scope')
@click.option('--tenant-id', help='Tenant scope')
def model_show(model_name, user_id, tenant_id):
    """Show detailed information about a model."""
    db = Database()
    
    try:
        client = LLMClient(db=db, user_id=user_id, tenant_id=tenant_id)
        model_config = client.router.registry.get(model_name)
        
        if not model_config:
            click.echo(f"❌ Model '{model_name}' not found")
            return
        
        click.echo(f"Model: {model_config.model_name}")
        click.echo("=" * 60)
        click.echo(f"Provider:        {model_config.provider.value}")
        click.echo(f"Context Window:  {model_config.context_window:,} tokens")
        click.echo(f"Price (Prompt):  ${model_config.price_per_1k_prompt:.4f} per 1K tokens")
        click.echo(f"Price (Completion): ${model_config.price_per_1k_completion:.4f} per 1K tokens")
        click.echo(f"RPM Limit:       {model_config.rpm_limit}")
        click.echo(f"TPM Limit:       {model_config.tpm_limit:,}")
        click.echo(f"Active:          {'Yes' if model_config.is_active else 'No'}")
        if model_config.fallback_to:
            click.echo(f"Fallback:        {model_config.fallback_to}")
        if model_config.tags:
            click.echo(f"Tags:            {', '.join(model_config.tags)}")
    except Exception as e:
        click.echo(f"❌ Failed to show model: {e}")


@model.command('update')
@click.argument('model_name')
@click.option('--price-prompt', type=float, help='Update price per 1K prompt tokens')
@click.option('--price-completion', type=float, help='Update price per 1K completion tokens')
@click.option('--rpm-limit', type=int, help='Update requests per minute limit')
@click.option('--tpm-limit', type=int, help='Update tokens per minute limit')
@click.option('--active/--inactive', default=None, help='Set model active status')
@click.option('--fallback', help='Update fallback model')
@click.option('--scope', type=click.Choice(['global', 'user', 'tenant']), default='global')
@click.option('--scope-id', help='User or tenant ID for scope')
def model_update(model_name, price_prompt, price_completion, rpm_limit, tpm_limit, 
                active, fallback, scope, scope_id):
    """Update model configuration."""
    db = Database()
    
    # Validate scope_id
    if scope in ['user', 'tenant'] and not scope_id:
        click.echo(f"❌ --scope-id is required for {scope} scope")
        return
    
    try:
        # Read existing registry
        query = "SELECT value FROM configs WHERE key_name = 'model_registry'"
        if scope == 'global':
            query += " AND scope_type = 'global'"
        elif scope == 'user':
            query += f" AND scope_type = 'user' AND scope_id = '{scope_id}'"
        elif scope == 'tenant':
            query += f" AND scope_type = 'tenant' AND scope_id = '{scope_id}'"
        
        row = db.fetchone(query)
        
        if not row:
            click.echo(f"❌ No model registry found for {scope}" + (f" ({scope_id})" if scope_id else ""))
            return
        
        existing = json.loads(row['value'])
        model_found = False
        
        for model in existing:
            if model['model_name'] == model_name:
                model_found = True
                # Update fields
                if price_prompt is not None:
                    model['price_per_1k_prompt'] = price_prompt
                if price_completion is not None:
                    model['price_per_1k_completion'] = price_completion
                if rpm_limit is not None:
                    model['rpm_limit'] = rpm_limit
                if tpm_limit is not None:
                    model['tpm_limit'] = tpm_limit
                if active is not None:
                    model['is_active'] = active
                if fallback is not None:
                    model['fallback_to'] = fallback
                break
        
        if not model_found:
            click.echo(f"❌ Model '{model_name}' not found in registry")
            return
        
        # Save updated registry
        if scope_id:
            db.execute(
                "UPDATE configs SET value = %s WHERE key_name = 'model_registry' AND scope_type = %s AND scope_id = %s",
                (json.dumps(existing), scope, scope_id)
            )
        else:
            db.execute(
                "UPDATE configs SET value = %s WHERE key_name = 'model_registry' AND scope_type = %s AND scope_id IS NULL",
                (json.dumps(existing), scope)
            )
        
        click.echo(f"✅ Model '{model_name}' updated successfully")
    except Exception as e:
        click.echo(f"❌ Failed to update model: {e}")


@cli.command()
@click.argument('session_id')
@click.option('--output', type=click.Choice(['text', 'json']), default='text')
def replay(session_id, output):
    """Replay a conversation session."""
    from core.replay.engine import ReplayEngine
    import json as json_lib
    
    db = Database()
    replay_engine = ReplayEngine(db)
    
    click.echo(f"🔄 Replaying session: {session_id}")
    click.echo("=" * 50)
    
    try:
        result = replay_engine.replay_session(session_id)
        
        if output == 'json':
            click.echo(json_lib.dumps(result, indent=2))
        else:
            click.echo(f"Session: {result['session_id']}")
            click.echo(f"Events replayed: {result['events_count']}")
            click.echo(f"Skills executed: {result['skills_executed']}")
            click.echo(f"Status: {result['status']}")
            
            if result.get('errors'):
                click.echo("\nErrors:")
                for error in result['errors']:
                    click.echo(f"  - {error}")
        
        click.echo("\n✅ Replay complete")
        
    except Exception as e:
        click.echo(f"❌ Replay failed: {e}", err=True)
        sys.exit(1)


@cli.group()
def session():
    """Manage sessions."""
    pass


@session.command('list')
@click.option('--user-id', help='Filter by user ID')
@click.option('--limit', default=10, help='Number of sessions to show')
def session_list(user_id, limit):
    """List recent sessions."""
    db = Database()
    
    query = "SELECT * FROM sessions"
    params = []
    
    if user_id:
        query += " WHERE user_id = %s"
        params.append(user_id)
    
    query += " ORDER BY created_at DESC LIMIT %s"
    params.append(limit)
    
    sessions = db.fetchall(query, tuple(params))
    
    if not sessions:
        click.echo("No sessions found")
        return
    
    click.echo("Recent Sessions:")
    click.echo("=" * 70)
    
    for s in sessions:
        status = "🟢" if s['status'] == 'active' else "⚪"
        click.echo(f"{status} {s['session_id']}")
        click.echo(f"   User: {s['user_id']}")
        click.echo(f"   Created: {s['created_at']}")
        click.echo(f"   Events: {s['event_count']}")
        click.echo()


@session.command('show')
@click.argument('session_id')
def session_show(session_id):
    """Show session details."""
    db = Database()
    
    # Get session
    session = db.fetchone(
        "SELECT * FROM sessions WHERE session_id = %s",
        (session_id,)
    )
    
    if not session:
        click.echo(f"Session not found: {session_id}", err=True)
        sys.exit(1)
    
    # Get events
    events = db.fetchall(
        "SELECT * FROM conversation_events WHERE session_id = %s ORDER BY created_at",
        (session_id,)
    )
    
    click.echo(f"Session: {session['session_id']}")
    click.echo("=" * 70)
    click.echo(f"User: {session['user_id']}")
    click.echo(f"Status: {session['status']}")
    click.echo(f"Created: {session['created_at']}")
    click.echo(f"Events: {len(events)}")
    click.echo()
    
    click.echo("Conversation:")
    click.echo("-" * 70)
    
    for event in events:
        prefix = "👤" if event['event_type'] == 'user_query' else "🤖"
        click.echo(f"{prefix} [{event['created_at']}]")
        click.echo(f"   {event['content'][:100]}...")
        click.echo()


@cli.command()
def init():
    """Initialize database schema."""
    import subprocess
    
    click.echo("🔧 Initializing database...")
    
    result = subprocess.run(
        ['make', 'db-init'],
        capture_output=True,
        text=True
    )
    
    if result.returncode == 0:
        click.echo("✅ Database initialized successfully")
    else:
        click.echo("❌ Database initialization failed", err=True)
        click.echo(result.stderr, err=True)
        sys.exit(1)


@cli.command()
def health():
    """Check system health."""
    db = Database()
    
    click.echo("🏥 System Health Check")
    click.echo("=" * 50)
    
    # Check database
    try:
        db.execute("SELECT 1")
        click.echo("✅ Database: Connected")
    except Exception as e:
        click.echo(f"❌ Database: {e}")
        sys.exit(1)
    
    # Check tables
    tables = ['sessions', 'conversation_events', 'skills_registry']
    for table in tables:
        try:
            count = db.fetchone(f"SELECT COUNT(*) as cnt FROM {table}")
            click.echo(f"✅ Table {table}: {count['cnt']} rows")
        except Exception as e:
            click.echo(f"❌ Table {table}: {e}")
    
    click.echo("\n✅ System healthy")


if __name__ == '__main__':
    cli()
