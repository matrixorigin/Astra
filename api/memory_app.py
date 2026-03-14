"""Memory Service — slim runtime that loads only memory-related modules.

Start with:
    uvicorn api.memory_app:memory_app --port 8100
"""

from contextlib import asynccontextmanager

from fastapi import FastAPI, HTTPException, Request
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse

from api.sse_errors import format_validation_error, is_sse_endpoint, sse_error_response
from core.logging_config import get_logger, setup_logging

setup_logging(level="INFO", json_format=False)
logger = get_logger(__name__)


def _init_memory_db() -> None:
    """Create only the tables needed by memory service."""
    from api.base import Base
    from api.database import engine
    from api.models.registry import ModelGroup, import_models_for_groups, MEMORY_SERVICE_GROUPS

    import_models_for_groups(MEMORY_SERVICE_GROUPS)
    # Also import skill knowledge models (used by memory retrieval)
    import_models_for_groups([ModelGroup.KNOWLEDGE])
    Base.metadata.create_all(bind=engine, checkfirst=True)

    # Vector indexes for memory tables only
    from matrixone import VectorIndex

    for tbl, idx_name, col_name in [
        ("mem_memories", "idx_memory_embedding", "embedding"),
        ("memory_graph_nodes", "idx_graph_node_embedding", "embedding"),
    ]:
        try:
            from sqlalchemy import text

            with engine.begin() as conn:
                idx_rows = conn.execute(text(f"SHOW INDEX FROM {tbl}")).fetchall()
                if not any(idx_name in str(r) for r in idx_rows):
                    VectorIndex.create_index(
                        engine,
                        table_name=tbl,
                        name=idx_name,
                        column=col_name,
                        index_type="ivfflat",
                        lists=10,
                        op_type="vector_l2_ops",
                    )
        except Exception as e:
            logger.debug("Vector index %s skipped: %s", idx_name, e)


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Startup / shutdown for memory service."""
    logger.info("Initializing memory service database...")
    try:
        _init_memory_db()
        logger.info("Memory service database initialized")
    except Exception as e:
        logger.warning(f"Database init skipped: {e}")

    # Memory governance scheduler (decay, quarantine, compression)
    from core.context.scheduler import MemoryGovernanceScheduler

    scheduler = MemoryGovernanceScheduler()
    await scheduler.start()

    # Seed RBAC roles
    try:
        from api.database import get_db_session
        from core.auth.seed_roles import seed_roles

        db = next(get_db_session())
        seeded = seed_roles(db)
        if seeded:
            logger.info(f"Seeded {seeded} RBAC role(s)")
        db.close()
    except Exception as e:
        logger.debug(f"Role seeding skipped: {e}")

    yield

    await scheduler.stop()
    logger.info("Memory service shut down")


memory_app = FastAPI(
    title="Memoria Lite",
    description="Memory service for AI coding tools — shared memory across Kiro, Cursor, Claude Code.",
    version="0.1.0",
    docs_url="/docs",
    redoc_url="/redoc",
    lifespan=lifespan,
)


# ── Exception handlers (same as main app) ────────────────────────────


@memory_app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    logger.error(f"Unhandled exception: {exc}", exc_info=True)
    if is_sse_endpoint(request.url.path):
        return sse_error_response(500, "Internal server error")
    return JSONResponse(status_code=500, content={"detail": "Internal server error"})


@memory_app.exception_handler(HTTPException)
async def http_exception_handler(request: Request, exc: HTTPException):
    if is_sse_endpoint(request.url.path):
        return sse_error_response(exc.status_code, exc.detail)
    return JSONResponse(
        status_code=exc.status_code, content={"detail": exc.detail}, headers=exc.headers
    )


@memory_app.exception_handler(RequestValidationError)
async def validation_exception_handler(request: Request, exc: RequestValidationError):
    if is_sse_endpoint(request.url.path):
        return sse_error_response(422, format_validation_error(exc))
    from fastapi.encoders import jsonable_encoder

    return JSONResponse(status_code=422, content={"detail": jsonable_encoder(exc.errors())})


# ── Middleware ─────────────────────────────────────────────────────────


@memory_app.middleware("http")
async def log_requests(request: Request, call_next):
    logger.info(f"{request.method} {request.url.path}")
    response = await call_next(request)
    logger.info(f"{request.method} {request.url.path} - {response.status_code}")
    return response


memory_app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


# ── Routers ───────────────────────────────────────────────────────────

from api.routers import auth, events, sessions, sandbox

memory_app.include_router(auth.router, prefix="/auth", tags=["authentication"])
memory_app.include_router(events.router, prefix="/events", tags=["events"])
memory_app.include_router(sessions.router, prefix="/sessions", tags=["sessions"])
memory_app.include_router(sandbox.router, tags=["sandbox"])

# Memory-specific REST API
from api.routers import memory as memory_router

memory_app.include_router(memory_router.router, prefix="/v1", tags=["memory"])


# ── Health ────────────────────────────────────────────────────────────


@memory_app.get("/health")
def health_check():
    from api.database import get_db_session
    from sqlalchemy import text

    db = next(get_db_session())
    try:
        db.execute(text("SELECT 1"))
        db_healthy = True
    except Exception:
        db_healthy = False

    return {
        "service": "memoria-lite",
        "status": "healthy" if db_healthy else "unhealthy",
        "database": "connected" if db_healthy else "disconnected",
    }


@memory_app.get("/")
def root():
    return {
        "name": "memoria-lite",
        "version": "0.1.0",
        "docs": "/docs",
    }
