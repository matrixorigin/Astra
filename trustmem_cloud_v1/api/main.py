"""TrustMem Cloud v1 — FastAPI application."""

from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from trustmem_cloud_v1.api.database import init_db


def _init_embedding() -> None:
    """Build EmbeddingClient from TrustMem config and inject into core singleton."""
    from trustmem_cloud_v1.config import get_settings
    from core.embedding import EmbeddingClient, set_embedding_client
    from core.embedding.client import KNOWN_DIMENSIONS

    s = get_settings()
    dim = s.embedding_dim
    if dim == 0:
        dim = KNOWN_DIMENSIONS.get(s.embedding_model, 1024)
    set_embedding_client(
        EmbeddingClient(
            provider=s.embedding_provider,
            model=s.embedding_model,
            dim=dim,
            api_key=s.embedding_api_key,
            base_url=s.embedding_base_url,
        )
    )


@asynccontextmanager
async def lifespan(app: FastAPI):
    # Inject embedding client from TrustMem config before anything else
    _init_embedding()

    # Warn about weak master key
    from trustmem_cloud_v1.config import get_settings
    import logging
    warning = get_settings().warn_weak_master_key()
    if warning:
        logging.getLogger("trustmem").warning(warning)

    init_db()

    # Start periodic governance scheduler (hourly/daily/weekly)
    from core.context.scheduler import GovernanceTaskRunner, AsyncIOBackend, MemoryGovernanceScheduler
    from trustmem_cloud_v1.api.database import get_db_context, get_db_factory
    runner = GovernanceTaskRunner(get_db_context, db_factory=get_db_factory(), memory_only=True)
    backend = AsyncIOBackend(runner)
    scheduler = MemoryGovernanceScheduler(backend=backend)
    await scheduler.start()

    yield

    await scheduler.stop()


app = FastAPI(
    title="TrustMem Cloud v1",
    description="Multi-tenant memory service with API key auth",
    version="0.1.0",
    lifespan=lifespan,
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"], allow_credentials=True,
    allow_methods=["*"], allow_headers=["*"],
)

from trustmem_cloud_v1.api.middleware import RateLimitMiddleware  # noqa: E402
app.add_middleware(RateLimitMiddleware)

from trustmem_cloud_v1.api.routers import auth, memory, snapshots, health, admin, user_ops  # noqa: E402

app.include_router(auth.router, prefix="/auth")
app.include_router(memory.router, prefix="/v1")
app.include_router(snapshots.router, prefix="/v1")
app.include_router(user_ops.router, prefix="/v1")
app.include_router(admin.router)
app.include_router(health.router)
