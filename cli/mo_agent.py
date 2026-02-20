#!/usr/bin/env python3
"""mo-agent CLI - Command-line interface for mo-agent-engine."""

import sys
from pathlib import Path

import click

# Add project root to path
sys.path.insert(0, str(Path(__file__).parent.parent))

import asyncio
import json

from core.agent.chat_loop import ChatLoop
from core.agent.executor import AgentExecutor
from core.context import ContextManager
from core.events.event_logger import EventLogger
from core.events.models import StreamEventType
from core.events.session_manager import SessionManager
from core.llm.client import LLMClient
from core.skills.builtin import register_builtin_skills
from core.skills.mocking import MockMode
from core.skills.pipeline import SkillPipeline
from core.skills.registry import SkillRegistry
from sqlalchemy.orm import Session
from api.database import get_db_session


@click.group()
@click.version_option(version="0.1.0")
def cli():
    """mo-agent - Event-centric intelligent agent platform."""
    pass


@cli.command()
@click.option("--user-id", default="cli_user", help="User identifier")
@click.option("--model", default="gpt-4", help="LLM model to use")
@click.option(
    "--mode",
    type=click.Choice(["production", "replay"]),
    default="production",
    help="Execution mode",
)
def chat(user_id, model, mode):
    """Start interactive chat session."""
    click.echo(f"🤖 mo-agent interactive chat (Mode: {mode})")
    click.echo("=" * 50)

    # Initialize
    db = next(get_db_session())
    session_mgr = SessionManager(db)
    logger = EventLogger(db)
    llm_client = LLMClient(db)
    context_mgr = ContextManager(db)

    # Create session first (needed for auditable selector)
    session = session_mgr.create_session(user_id=user_id)
    click.echo(f"Session: {session.session_id}")
    click.echo("Type 'exit' or 'quit' to end session\n")

    # Initialize firewall
    from core.verification.firewall import HallucinationFirewall

    firewall = HallucinationFirewall(db, context_mgr, threshold=0.7)

    # Agent Components (with unified pipeline)
    pipeline = SkillPipeline(db, llm_client, audit=True, learning=True)
    executor = AgentExecutor(
        db=db,
        registry=None,  # Will be set after skill registration
        mode=MockMode(mode),
        pipeline=pipeline,  # Pass pipeline for automatic feedback recording
    )
    chat_loop = ChatLoop(
        selector=pipeline,
        executor=executor,
        llm_client=llm_client,
        event_logger=logger,
        context_manager=context_mgr,
        firewall=firewall,
    )

    # Create agent registry with example agents
    from core.agent.agent_registry import AgentProfile, AgentRegistry

    agent_registry = AgentRegistry()

    # Register example agents
    agent_registry.register(
        AgentProfile(
            agent_id="code_reviewer",
            system_prompt="You are a code review expert. Analyze code for bugs, style issues, and performance problems. Provide constructive feedback.",
            skill_filter=["summarize_pr", "analyze_code"],
        )
    )
    agent_registry.register(
        AgentProfile(
            agent_id="security_auditor",
            system_prompt="You are a security auditor. Review code for security vulnerabilities, input validation issues, and potential exploits.",
            skill_filter=["summarize_pr", "analyze_code"],
        )
    )
    agent_registry.register(
        AgentProfile(
            agent_id="documentation_writer",
            system_prompt="You are a documentation writer. Create clear, comprehensive documentation for code, APIs, and features.",
            skill_filter=["summarize_pr", "write_docs"],
        )
    )

    # Create chat_loop_factory for delegation (also with auditable selector)
    def create_chat_loop(system_prompt=None, agent_id="dev-agent"):
        from core.context.manager import ContextManager
        from core.verification.firewall import HallucinationFirewall
        
        return ChatLoop(
            selector=SkillPipeline(db, llm_client, audit=True, learning=True),
            executor=AgentExecutor(db=db, registry=SkillRegistry(db), mode=MockMode(mode)),
            llm_client=llm_client,
            event_logger=EventLogger(db),
            context_manager=ContextManager(db),
            firewall=HallucinationFirewall(db),
            agent_id=agent_id,
        )

    # Register skills with agent registry
    skill_registry = SkillRegistry(db)
    register_builtin_skills(
        skill_registry, db, agent_registry=agent_registry, chat_loop_factory=create_chat_loop
    )

    # Update executor with registered skills
    executor.registry = skill_registry

    # Set user context for LLM client (for scope-based access control)
    llm_client.set_user_context(user_id=user_id)

    try:
        while True:
            # Get user input
            user_input = click.prompt("You", type=str, prompt_suffix="> ")

            if user_input.lower() in ["exit", "quit"]:
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


@skill.command("list")
@click.option("--active-only", is_flag=True, help="Show only active skills")
def skill_list(active_only):
    """List available skills."""
    db = next(get_db_session())

    # Query skills directly
    from sqlalchemy import text
    
    query = "SELECT * FROM skills_registry"
    if active_only:
        query += " WHERE is_active = TRUE"
    query += " ORDER BY skill_name, version DESC"

    result = db.execute(text(query))
    skills = [dict(row._mapping) for row in result]

    if not skills:
        click.echo("No skills registered")
        return

    click.echo("Available Skills:")
    click.echo("=" * 70)

    for skill in skills:
        status = "✓" if skill.get("is_active", True) else "✗"
        click.echo(f"{status} {skill['skill_name']} v{skill['version']}")
        click.echo(f"  {skill['description']}")
        click.echo()


@skill.command("register")
@click.argument("skill_file", type=click.Path(exists=True))
def skill_register(skill_file):
    """Register a new skill from file."""

    db = next(get_db_session())
    registry = SkillRegistry(db)

    with open(skill_file) as f:
        skill_data = json.load(f)

    skill_id = registry.register(
        name=skill_data["name"],
        version=skill_data["version"],
        description=skill_data["description"],
        input_schema=skill_data["input_schema"],
        output_schema=skill_data["output_schema"],
        implementation=skill_data["implementation"],
    )

    click.echo(f"✅ Skill registered: {skill_data['name']} v{skill_data['version']}")
    click.echo(f"   Skill ID: {skill_id}")


@skill.command("learn")
@click.option("--days", default=7, help="Look back N days for failures")
@click.option("--force", is_flag=True, help="Force learning even in cooldown period")
def skill_learn(days, force):
    """Learn from historical skill selection failures with regression gating."""
    from core.llm.client import LLMClient
    from core.skills.pipeline import SkillPipeline
    
    click.echo(f"🧠 Learning from failures (last {days} days)...")
    if force:
        click.echo("⚡ Force mode enabled - bypassing cooldown")
    click.echo("=" * 70)
    
    db = next(get_db_session())
    llm_client = LLMClient(db)
    
    pipeline = SkillPipeline(db, llm_client, audit=False, learning=True)
    
    try:
        learn_result = pipeline.learn(days=days)
        
        if learn_result.error:
            click.echo(f"\n❌ Error: {learn_result.error}")
            raise click.Abort()
        
        click.echo("\n✅ Learning cycle completed:")
        click.echo(f"   Total failures: {learn_result.total_failures}")
        click.echo(f"   Learnings applied: {learn_result.learned}")
        
        if learn_result.gate_verdict != "skipped":
            verdict_icon = "✅" if learn_result.gate_verdict == "pass" else "❌"
            click.echo(f"\n🚪 Regression Gate: {verdict_icon} {learn_result.gate_verdict.upper()}")
            click.echo(f"   Improvement: {learn_result.improvement_pct:.1f}%")
            
            if result['gate_verdict'] == 'fail':
                click.echo("\n⚠️  Learning rejected by regression gate - no changes deployed")
                return
        
        # Show learning stats
        stats = pipeline.stats()
        click.echo(f"\n📊 Learning Statistics:")
        click.echo(f"   Total learnings: {stats.get('total_learnings', 0)}")
        click.echo(f"   High confidence: {stats.get('high_confidence', 0)}")
        click.echo(f"   Avg confidence: {stats.get('avg_confidence', 0):.1f}")
        
    except Exception as e:
        click.echo(f"❌ Learning failed: {e}", err=True)
        import traceback
        traceback.print_exc()
        raise click.Abort()


@skill.command("gate")
@click.argument("version")
@click.option("--min-improvement", default=-0.05, help="Minimum improvement threshold")
def skill_gate(version, min_improvement):
    """Run regression gate for skill selector changes."""
    from core.evaluation.regression_gate import RegressionGate, ChangeType
    
    click.echo(f"🚪 Running regression gate for selector {version}...")
    
    db = next(get_db_session())
    gate = RegressionGate(db)
    
    try:
        result = gate.validate_change(
            change_type=ChangeType.SELECTOR,
            change_id=version,
            change_content={"version": version},
            score_regression_threshold=min_improvement,
        )
        
        verdict = result["verdict"]
        metrics = result.get("metrics", {})
        click.echo(f"\n{'✅' if verdict == 'pass' else '❌'} {verdict}")
        click.echo(f"   Sessions tested: {result['sessions_tested']}")
        click.echo(f"   Error rate: {metrics.get('error_rate', 0):.2%}")
        click.echo(f"   Score delta: {metrics.get('score_delta', 0):.2f}")
        
        if verdict != 'pass':
            raise click.Abort()
            
    except click.Abort:
        raise
    except Exception as e:
        click.echo(f"❌ Gate failed: {e}", err=True)
        raise click.Abort()


@skill.command("history")
@click.option("--session", help="Filter by session ID")
@click.option("--limit", default=20, help="Number of records to show")
def skill_history(session, limit):
    """View skill selection history with audit trail."""
    from core.llm.client import LLMClient
    
    db = next(get_db_session())
    llm_client = LLMClient(db)
    pipeline = SkillPipeline(db, llm_client, audit=False, learning=False)
    
    try:
        events = pipeline.selection_history(session_id=session, limit=limit)
        
        if not events:
            click.echo("No selection history found")
            return
        
        click.echo(f"\n📜 Skill Selection History ({len(events)} records)")
        click.echo("=" * 80)
        
        for event in events:
            click.echo(f"\n🔍 {event['event_id'][:8]}... | {event['created_at']}")
            click.echo(f"   Query: {event['user_query'][:60]}...")
            click.echo(f"   Selected: {event['selected_skills']}")
            click.echo(f"   Method: {event['selection_method']}")
                
    except Exception as e:
        click.echo(f"❌ Failed to get history: {e}", err=True)
        raise click.Abort()


@skill.command("stats")
def skill_stats():
    """Show skill selection learning statistics."""
    from core.llm.client import LLMClient
    from core.evaluation.regression_gate import RegressionGate
    from core.skills.self_improving_selector import SelfImprovingSelector
    
    db = next(get_db_session())
    llm_client = LLMClient(db)
    
    learner = SelfImprovingSelector(db, llm_client)
    learning_stats = learner.get_learning_stats()
    
    gate = RegressionGate(db)
    gate_stats_raw = gate.get_gate_history(limit=100)
    total = len(gate_stats_raw)
    passed = sum(1 for r in gate_stats_raw if r.get("passed"))
    failed = total - passed
    
    click.echo("\n📊 Skill Selection Statistics")
    click.echo("=" * 80)
    
    click.echo("\n🧠 Learning:")
    click.echo(f"   Total learnings: {learning_stats['total_learnings']}")
    click.echo(f"   Avg confidence: {learning_stats['avg_confidence']:.2f}")
    click.echo(f"   Total evidence: {learning_stats['total_evidence']}")
    click.echo(f"   Total applications: {learning_stats['total_applications']}")
    click.echo(f"   High confidence: {learning_stats['high_confidence_learnings']}")
    
    click.echo("\n🚪 Regression Gates:")
    click.echo(f"   Total gates: {total}")
    click.echo(f"   Passed: {passed}")
    click.echo(f"   Failed: {failed}")
    click.echo(f"   Pass rate: {passed / total if total else 0:.1%}")


@cli.group()
def model():
    """Manage LLM models."""
    pass


@model.command("list")
@click.option("--user-id", help="Filter by user scope")
@click.option("--tenant-id", help="Filter by tenant scope")
def model_list(user_id, tenant_id):
    """List available models."""
    db = next(get_db_session())

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
        click.echo(
            f"   Price: ${m.price_per_1k_prompt:.4f}/1K prompt + ${m.price_per_1k_completion:.4f}/1K completion"
        )
        if m.fallback_to:
            click.echo(f"   Fallback: {m.fallback_to}")
        click.echo()


# Model management commands removed - use mo-admin CLI instead
# Regular users can only view models, not modify them


@model.command("show")
@click.argument("model_name")
@click.option("--user-id", help="User scope")
@click.option("--tenant-id", help="Tenant scope")
def model_show(model_name, user_id, tenant_id):
    """Show detailed information about a model."""
    db = next(get_db_session())

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

    # Model update command removed - use mo-admin CLI instead
    except Exception as e:
        click.echo(f"❌ Failed to update model: {e}")


@cli.command()
@click.argument("session_id")
@click.option("--output", type=click.Choice(["text", "json"]), default="text")
def replay(session_id, output):
    """Replay a conversation session."""
    import json as json_lib

    from core.replay.engine import ReplayEngine

    db = next(get_db_session())
    replay_engine = ReplayEngine(db)

    click.echo(f"🔄 Replaying session: {session_id}")
    click.echo("=" * 50)

    try:
        result = replay_engine.replay_session(session_id)

        if output == "json":
            click.echo(json_lib.dumps(result, indent=2))
        else:
            click.echo(f"Session: {result['session_id']}")
            click.echo(f"Events replayed: {result['events_count']}")
            click.echo(f"Skills executed: {result['skills_executed']}")
            click.echo(f"Status: {result['status']}")

            if result.get("errors"):
                click.echo("\nErrors:")
                for error in result["errors"]:
                    click.echo(f"  - {error}")

        click.echo("\n✅ Replay complete")

    except Exception as e:
        click.echo(f"❌ Replay failed: {e}", err=True)
        sys.exit(1)


@cli.group()
def session():
    """Manage sessions."""
    pass


@session.command("list")
@click.option("--user-id", help="Filter by user ID")
@click.option("--limit", default=10, help="Number of sessions to show")
def session_list(user_id, limit):
    """List recent sessions."""
    from sqlalchemy import text
    
    db = next(get_db_session())

    query = "SELECT * FROM sessions"
    params = {}

    if user_id:
        query += " WHERE user_id = :user_id"
        params["user_id"] = user_id

    query += f" ORDER BY created_at DESC LIMIT {limit}"

    result = db.execute(text(query), params)
    sessions = [dict(row._mapping) for row in result]

    if not sessions:
        click.echo("No sessions found")
        return

    click.echo("Recent Sessions:")
    click.echo("=" * 70)

    for s in sessions:
        status = "🟢" if s["status"] == "active" else "⚪"
        click.echo(f"{status} {s['session_id']}")
        click.echo(f"   User: {s['user_id']}")
        click.echo(f"   Created: {s['created_at']}")
        click.echo(f"   Events: {s['event_count']}")
        click.echo()


@session.command("show")
@click.argument("session_id")
def session_show(session_id):
    """Show session details."""
    from api.models import Session as SessionModel, Event
    from sqlalchemy import text
    
    db = next(get_db_session())

    # Get session using ORM
    session = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()

    if not session:
        click.echo(f"Session not found: {session_id}", err=True)
        sys.exit(1)

    # Get events using ORM
    events = db.query(Event).filter(Event.session_id == session_id).order_by(Event.created_at).all()

    click.echo(f"Session: {session.session_id}")
    click.echo("=" * 70)
    click.echo(f"User: {session.user_id}")
    click.echo(f"Status: {session['status']}")
    click.echo(f"Created: {session['created_at']}")
    click.echo(f"Events: {len(events)}")
    click.echo()

    click.echo("Conversation:")
    click.echo("-" * 70)

    for event in events:
        prefix = "👤" if event["event_type"] == "user_query" else "🤖"
        click.echo(f"{prefix} [{event['created_at']}]")
        click.echo(f"   {event['content'][:100]}...")
        click.echo()


@cli.command()
def init():
    """Initialize database schema."""
    import subprocess

    click.echo("🔧 Initializing database...")

    result = subprocess.run(["make", "db-init"], capture_output=True, text=True)

    if result.returncode == 0:
        click.echo("✅ Database initialized successfully")
    else:
        click.echo("❌ Database initialization failed", err=True)
        click.echo(result.stderr, err=True)
        sys.exit(1)


@cli.command()
def health():
    """Check system health."""
    db = next(get_db_session())

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
    tables = ["sessions", "conversation_events", "skills_registry"]
    for table in tables:
        try:
            from sqlalchemy import text
            result = db.execute(text(f"SELECT COUNT(*) as cnt FROM {table}")).first()
            click.echo(f"✅ Table {table}: {result.cnt} rows")
        except Exception as e:
            click.echo(f"❌ Table {table}: {e}")

    click.echo("\n✅ System healthy")


if __name__ == "__main__":
    cli()
