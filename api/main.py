"""FastAPI main application."""

from contextlib import asynccontextmanager

from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse

from api.database import init_db
from api.routers import (
    admin,
    agents,
    auth,
    context,
    decisions,
    events,
    marketplace,
    replay,
    sandbox,
    sessions,
    skills,
    streaming,
)
from core.logging_config import get_logger

logger = get_logger(__name__)


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Lifespan context manager for startup/shutdown events."""
    # Startup
    logger.info("Initializing database...")
    try:
        init_db()
        logger.info("Database initialized successfully")
    except Exception as e:
        logger.warning(f"Database init skipped (tables may already exist): {e}")

    # Start memory governance scheduler
    from core.context.scheduler import MemoryGovernanceScheduler
    scheduler = MemoryGovernanceScheduler()
    await scheduler.start()

    # Restore workflows that were waiting when process died
    from core.agent.async_tools import cleanup_stale_workflows, restore_waiting_workflows
    restored = restore_waiting_workflows()
    if restored:
        logger.info(f"Restored {restored} waiting workflow(s)")

    # Seed predefined agent roles
    try:
        from api.database import get_db_session
        from core.agent.seed_agents import seed_agents
        db = next(get_db_session())
        seeded = seed_agents(db)
        if seeded:
            logger.info(f"Seeded {seeded} agent role(s)")
        db.close()
    except Exception as e:
        logger.debug(f"Agent seeding skipped: {e}")

    # Periodic workflow cleanup (every hour)
    import asyncio
    async def _cleanup_loop():
        while True:
            await asyncio.sleep(3600)
            await cleanup_stale_workflows(max_age_hours=24)
    cleanup_task = asyncio.create_task(_cleanup_loop())

    # Cron trigger scheduler (check every 30s)
    async def _trigger_loop():
        while True:
            await asyncio.sleep(30)
            try:
                from api.database import get_db_session as _get_db
                from core.agent.triggers import claim_and_advance, fire_trigger, get_due_triggers
                db = next(_get_db())
                try:
                    due = get_due_triggers(db)
                    for tid in due:
                        try:
                            if claim_and_advance(db, tid):
                                fire_trigger(db, tid)
                        except Exception as e:
                            logger.warning(f"Trigger {tid} fire failed: {e}")
                finally:
                    db.close()
            except Exception as e:
                logger.debug(f"Trigger loop error: {e}")
    trigger_task = asyncio.create_task(_trigger_loop())

    yield

    # Shutdown
    cleanup_task.cancel()
    trigger_task.cancel()
    await scheduler.stop()

    # Graceful job backend shutdown — wait for subprocess cleanup
    from api.routers.jobs import _router as job_router
    await job_router.shutdown()

    logger.info("Shutting down...")


app = FastAPI(
    title="Agent Engine API",
    description="""
    Universal agent state management platform with authentication, session tracking, and event logging.
    
    ## Features
    
    * **Authentication**: JWT-based auth with access/refresh tokens
    * **Agent Management**: CRUD operations for AI agents
    * **Session Management**: Conversation lifecycle tracking
    * **Event Logging**: Record and query conversation events
    
    ## Authentication
    
    Most endpoints require authentication. Use `/auth/login` to get access token, 
    then include it in the `Authorization: Bearer <token>` header.
    
    ## Quick Start
    
    1. Register: `POST /auth/register`
    2. Login: `POST /auth/login` 
    3. Create agent: `POST /agents`
    4. Create session: `POST /sessions`
    5. Log events: `POST /events`
    """,
    version="0.1.0",
    docs_url="/docs",
    redoc_url="/redoc",
    lifespan=lifespan,
)


# Global exception handler
@app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    """Handle unexpected exceptions."""
    logger.error(f"Unhandled exception: {exc}", exc_info=True)
    return JSONResponse(
        status_code=500,
        content={"detail": "Internal server error"},
    )


# Request logging middleware
@app.middleware("http")
async def log_requests(request: Request, call_next):
    """Log all requests."""
    logger.info(f"{request.method} {request.url.path}")
    response = await call_next(request)
    logger.info(f"{request.method} {request.url.path} - {response.status_code}")
    return response


# CORS middleware
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],  # Configure in production
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Include routers
app.include_router(auth.router, prefix="/auth", tags=["authentication"])
app.include_router(agents.router, prefix="/agents", tags=["agents"])
app.include_router(sessions.router, prefix="/sessions", tags=["sessions"])
app.include_router(events.router, prefix="/events", tags=["events"])
app.include_router(sandbox.router, tags=["sandbox"])
app.include_router(replay.router, tags=["replay"])
app.include_router(skills.router, prefix="/skills", tags=["skills"])
app.include_router(marketplace.router, prefix="/marketplace", tags=["marketplace"])
app.include_router(context.router, prefix="/context", tags=["context"])
app.include_router(decisions.router, prefix="/decisions", tags=["decisions"])
app.include_router(streaming.router, tags=["streaming (deprecated)"])

# Chat API — unified conversation entry point
from api.routers import chat

app.include_router(chat.router, tags=["chat"])

from api.routers import jobs

app.include_router(jobs.router, tags=["jobs"])

from api.routers import workflows

app.include_router(workflows.router, tags=["workflows"])

# Learning service API
from api.routers import learning

app.include_router(learning.router, tags=["learning"])

# Evaluation — quality trends, drift, gate history, calibration
from api.routers import evaluation

app.include_router(evaluation.router, tags=["evaluation"])

# Branches — zero-copy data branching (diff, merge, cost estimation)
from api.routers import branches

app.include_router(branches.router, tags=["branches"])

# Triggers — webhook + cron → AgentRun
from api.routers import triggers

app.include_router(triggers.router, tags=["triggers"])

# Data Versioning — checkpoints, lineage, sandbox checkpoint/restore
from api.routers import data_versioning

app.include_router(data_versioning.router)

# Admin — system management (requires admin role)
app.include_router(admin.router, tags=["admin"])

# Models — model management
from api.routers import models

app.include_router(models.router, tags=["models"])


@app.get("/health")
def health_check():
    """Health check endpoint."""
    from api.database import get_db_session

    db = next(get_db_session())
    # Simple health check - try to execute a query
    try:
        from sqlalchemy import text
        db.execute(text("SELECT 1"))
        db_healthy = True
    except Exception:
        db_healthy = False

    return {
        "status": "healthy" if db_healthy else "unhealthy",
        "database": "connected" if db_healthy else "disconnected",
    }


@app.get("/")
def root():
    """Root endpoint."""
    return {
        "name": "Agent Engine API",
        "version": "0.1.0",
        "docs": "/docs",
    }
