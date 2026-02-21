"""FastAPI main application."""

from contextlib import asynccontextmanager

from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse

from api.database import init_db
from api.routers import agents, auth, events, sessions, sandbox, replay, skills, context, decisions, streaming
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

    yield

    # Shutdown
    await scheduler.stop()
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
app.include_router(context.router, prefix="/context", tags=["context"])
app.include_router(decisions.router, prefix="/decisions", tags=["decisions"])
app.include_router(streaming.router, tags=["streaming (deprecated)"])

# Chat API — unified conversation entry point
from api.routers import chat
app.include_router(chat.router, tags=["chat"])

from api.routers import jobs
app.include_router(jobs.router, tags=["jobs"])

# Learning service API
from api.routers import learning
app.include_router(learning.router, tags=["learning"])


@app.get("/health")
def health_check():
    """Health check endpoint."""
    from sqlalchemy.orm import Session
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
