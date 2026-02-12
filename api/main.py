"""FastAPI main application."""

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from api.routers import agents, auth, events, sessions
from core.logging_config import get_logger

logger = get_logger(__name__)

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
)

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


@app.get("/health")
def health_check():
    """Health check endpoint."""
    from sdk import Database
    
    db = Database()
    # Simple health check - try to connect
    try:
        with db.get_connection():
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
