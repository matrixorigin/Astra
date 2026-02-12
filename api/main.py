"""FastAPI main application."""

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from api.routers import agents, auth, events, sessions
from core.logging_config import get_logger

logger = get_logger(__name__)

app = FastAPI(
    title="Agent Engine API",
    description="Universal agent state management platform",
    version="0.1.0",
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
