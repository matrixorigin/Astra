"""Embedding service — thin wrapper around EmbeddingClient for DB operations.

The actual embedding generation is delegated to EmbeddingClient (core/embedding/).
This module adds store_embedding() and search_similar() which need DB access.
"""

from typing import Any

from config.settings import get_settings
from core.db_consumer import DbConsumer, DbFactory
from core.embedding.client import EmbeddingClient
from core.logging_config import get_logger

logger = get_logger(__name__)

# Process-wide singleton — created once, reused everywhere.
# Delegates to canonical singleton in core.embedding (foundation layer).
_shared_client: EmbeddingClient | None = None


def get_embedding_client() -> EmbeddingClient:
    """Get or create the process-wide EmbeddingClient singleton.

    Delegates to :func:`core.embedding.get_embedding_client`.
    """
    from core.embedding import get_embedding_client as _get

    return _get()


class EmbeddingService(DbConsumer):
    """Embedding service with DB operations (store/search).

    Delegates embed_text() to the shared EmbeddingClient singleton.
    Kept for backward compatibility with callers that need DB access.
    """

    def __init__(self, db_factory: DbFactory, provider: str | None = None, **_kwargs):
        super().__init__(db_factory)
        # provider param is accepted for backward compat but ignored —
        # the actual provider comes from config via get_embedding_client().
        # Exception: "mock" is honored for tests that explicitly request it.
        if provider == "mock":
            s = get_settings()
            self._client = EmbeddingClient(provider="mock", model="mock", dim=s.embedding_dim)
        else:
            self._client = get_embedding_client()

    @property
    def DIMENSION(self) -> int:
        return self._client.dimension

    @property
    def model(self) -> str:
        return self._client.model_name

    @property
    def provider(self) -> str:
        return self._client.model_name  # backward compat

    def embed_text(self, text: str) -> list[float]:
        return self._client.embed(text)

    def store_embedding(
        self, event_id: str, embedding: list[float], metadata: dict[str, Any] | None = None
    ):
        dim = self._client.dimension
        if len(embedding) != dim:
            raise ValueError(f"Embedding must be {dim} dimensions, got {len(embedding)}")
        vec_str = "[" + ",".join(str(x) for x in embedding) + "]"
        from sqlalchemy.dialects.mysql import insert

        from api.models.context import EventEmbedding

        _meta = EventEmbedding.__table__.c.metadata
        with self._db() as db:
            stmt = (
                insert(EventEmbedding.__table__)
                .values(
                    event_id=event_id,
                    embedding=vec_str,
                    model_name=self._client.model_name,
                    model_version="1.0",
                    **{_meta.name: metadata or {}},
                )
                .on_duplicate_key_update(
                    embedding=vec_str,
                    **{_meta.name: metadata or {}},
                )
            )
            db.execute(stmt)
            db.commit()

    def search_similar(
        self,
        query_embedding: list[float],
        limit: int = 10,
        session_id: str | None = None,
        filters: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        dim = self._client.dimension
        if len(query_embedding) != dim:
            raise ValueError(f"Query must be {dim} dimensions, got {len(query_embedding)}")

        from matrixone.sqlalchemy_ext import l2_distance

        from api.models.agent import Event
        from api.models.context import EventEmbedding

        dist = l2_distance(EventEmbedding.embedding, query_embedding)
        dist_expr = dist.label("distance")
        sim_expr = (1.0 / (1.0 + dist)).label("similarity")

        with self._db() as db:
            query = db.query(
                Event.event_id,
                Event.session_id,
                Event.content,
                Event.event_type,
                Event.created_at,
                dist_expr,
                sim_expr,
            ).join(EventEmbedding, Event.event_id == EventEmbedding.event_id)
            if session_id:
                query = query.filter(Event.session_id == session_id)
            if filters:
                from matrixone.sqlalchemy_ext import json_extract_string

                for key, value in filters.items():
                    if key == "event_type":
                        query = query.filter(Event.event_type == value)
                    else:
                        query = query.filter(
                            json_extract_string(EventEmbedding.embedding_metadata, f"$.{key}")
                            == value
                        )
            query = query.order_by("distance").limit(limit)
            return [dict(row._mapping) for row in query.all()]
