"""FastAPI application with health checks and error handling."""

from fastapi import FastAPI, Request, status, Depends
from fastapi.responses import JSONResponse
from fastapi.middleware.cors import CORSMiddleware
from datetime import datetime, UTC
from uuid_utils import uuid7

from core.config import get_settings
from core.logging_config import setup_logging, get_logger
from core.exceptions import AgentError, AuthenticationError, AuthorizationError
from core.auth import api_key_auth, jwt_auth
from core.rate_limit import rate_limiter

# Conditional import for metrics
try:
    from core.metrics import metrics_endpoint
    METRICS_ENABLED = True
except ImportError:
    METRICS_ENABLED = False

# Load settings
settings = get_settings()

# Setup logging
setup_logging(level=settings.log_level, json_format=(settings.log_format == "json"))
logger = get_logger(__name__)

logger.info(f"Starting mo-dev-agent v{settings.version} in {settings.environment} mode")

# Create app
app = FastAPI(
    title="mo-dev-agent",
    description="Event-centric intelligent agent platform",
    version=settings.version,
    debug=settings.debug,
)

# CORS middleware
app.add_middleware(
    CORSMiddleware,
    allow_origins=settings.security.allowed_origins,
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Rate limiting middleware
if not settings.is_development():
    app.middleware("http")(rate_limiter)


# Request ID middleware
@app.middleware("http")
async def add_request_id(request: Request, call_next):
    """Add request ID to all requests."""
    request_id = str(uuid7())
    request.state.request_id = request_id
    
    logger.info(
        f"Request started: {request.method} {request.url.path}",
        extra={"request_id": request_id},
    )
    
    response = await call_next(request)
    response.headers["X-Request-ID"] = request_id
    
    logger.info(
        f"Request completed: {request.method} {request.url.path} - {response.status_code}",
        extra={"request_id": request_id},
    )
    
    return response


# Global exception handler
@app.exception_handler(AuthenticationError)
async def authentication_error_handler(request: Request, exc: AuthenticationError):
    """Handle authentication errors."""
    request_id = getattr(request.state, "request_id", "unknown")
    
    logger.warning(
        f"Authentication error: {exc.message}",
        extra={"request_id": request_id},
    )
    
    return JSONResponse(
        status_code=status.HTTP_401_UNAUTHORIZED,
        content={
            "error": exc.code,
            "message": exc.message,
            "request_id": request_id,
        },
    )


@app.exception_handler(AuthorizationError)
async def authorization_error_handler(request: Request, exc: AuthorizationError):
    """Handle authorization errors."""
    request_id = getattr(request.state, "request_id", "unknown")
    
    logger.warning(
        f"Authorization error: {exc.message}",
        extra={"request_id": request_id},
    )
    
    return JSONResponse(
        status_code=status.HTTP_403_FORBIDDEN,
        content={
            "error": exc.code,
            "message": exc.message,
            "request_id": request_id,
        },
    )


@app.exception_handler(AgentError)
async def agent_error_handler(request: Request, exc: AgentError):
    """Handle AgentError exceptions."""
    request_id = getattr(request.state, "request_id", "unknown")
    
    logger.error(
        f"Agent error: {exc.message}",
        extra={"request_id": request_id, "error_code": exc.code},
        exc_info=True,
    )
    
    return JSONResponse(
        status_code=status.HTTP_400_BAD_REQUEST,
        content={
            "error": exc.code,
            "message": exc.message,
            "request_id": request_id,
        },
    )


@app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    """Handle all unhandled exceptions."""
    request_id = getattr(request.state, "request_id", "unknown")
    
    logger.error(
        f"Unhandled exception: {str(exc)}",
        extra={"request_id": request_id},
        exc_info=True,
    )
    
    return JSONResponse(
        status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
        content={
            "error": "INTERNAL_ERROR",
            "message": "An internal error occurred",
            "request_id": request_id,
        },
    )


# Health check endpoints
@app.get("/health", tags=["Health"])
async def health_check():
    """Basic health check."""
    return {
        "status": "healthy",
        "version": settings.version,
        "environment": settings.environment,
        "timestamp": datetime.now(UTC).isoformat(),
    }


@app.get("/health/ready", tags=["Health"])
async def readiness_check():
    """Readiness check - verify dependencies."""
    from sdk import Database
    
    checks = {
        "database": "unknown",
        "timestamp": datetime.now(UTC).isoformat(),
    }
    
    # Check database
    try:
        db = Database()
        db.execute("SELECT 1")
        checks["database"] = "healthy"
    except Exception as e:
        logger.error(f"Database health check failed: {e}")
        checks["database"] = "unhealthy"
        return JSONResponse(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            content={"status": "not_ready", "checks": checks},
        )
    
    return {"status": "ready", "checks": checks}


@app.get("/health/live", tags=["Health"])
async def liveness_check():
    """Liveness check - verify app is running."""
    return {
        "status": "alive",
        "timestamp": datetime.now(UTC).isoformat(),
    }


# Root endpoint
@app.get("/", tags=["Root"])
async def root():
    """Root endpoint."""
    return {
        "name": "mo-dev-agent",
        "version": settings.version,
        "environment": settings.environment,
        "description": "Event-centric intelligent agent platform",
        "docs": "/docs",
        "health": "/health",
    }


# Protected endpoint example
@app.get("/api/protected", tags=["API"])
async def protected_endpoint(user_info: dict = Depends(api_key_auth.verify)):
    """Protected endpoint requiring API key.
    
    Example:
        curl -H "X-API-Key: your-key" http://localhost:8000/api/protected
    """
    return {
        "message": "Access granted",
        "user_id": user_info["user_id"],
        "permissions": user_info["permissions"],
    }


# JWT token endpoint
@app.post("/api/token", tags=["API"])
async def create_token(user_id: str, permissions: list[str] = None):
    """Create JWT token.
    
    Example:
        curl -X POST "http://localhost:8000/api/token?user_id=alice&permissions=read&permissions=write"
    """
    token = jwt_auth.create_token(user_id, permissions)
    return {
        "access_token": token,
        "token_type": "bearer",
        "expires_in": settings.security.jwt_expiration,
    }


# Metrics endpoint
if METRICS_ENABLED and settings.monitoring.enable_metrics:
    @app.get("/metrics", tags=["Monitoring"])
    async def metrics():
        """Prometheus metrics endpoint."""
        return await metrics_endpoint()


if __name__ == "__main__":
    import uvicorn
    
    uvicorn.run(
        "api.main:app",
        host=settings.host,
        port=settings.port,
        workers=settings.workers,
        reload=settings.debug,
        log_config=None,  # Use our custom logging
    )
