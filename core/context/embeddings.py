"""Embedding service — thin wrapper around EmbeddingClient for DB operations.

The actual embedding generation is delegated to EmbeddingClient (core/embedding/).
This module adds store_embedding() and search_similar() which need DB access.
"""

import json
from typing import Any

from config.settings import get_settings
from core.db_consumer import DbConsumer, DbFactory
from core.embedding.client import EmbeddingClient
from core.logging_config import get_logger

logger = get_logger(__name__)

# Process-wide singleton — created once, reused everywhere.
_shared_client: EmbeddingClient | None = None


def get_embedding_client() -> EmbeddingClient:
    """Get or create the process-wide EmbeddingClient singleton.

    Fails fast if the configured provider is unavailable (e.g., sentence-transformers
    not installed for local, or API key missing for openai).
    """
    global _shared_client
    if _shared_client is None:
        s = get_settings()
        _shared_client = EmbeddingClient(provider=s.embedding_provider, model=s.embedding_model, dim=s.embedding_dim)
        logger.info("EmbeddingClient: provider=%s, model=%s, dim=%d", s.embedding_provider, s.embedding_model, s.embedding_dim)
    return _shared_client


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

    def store_embedding(self, event_id: str, embedding: list[float], metadata: dict[str, Any] | None = None):
        dim = self._client.dimension
        if len(embedding) != dim:
            raise ValueError(f"Embedding must be {dim} dimensions, got {len(embedding)}")
        vec_str = "[" + ",".join(str(x) for x in embedding) + "]"
        metadata_json = json.dumps(metadata or {})
        from sqlalchemy import text
        with self._db() as db:
            db.execute(
                text("""
                INSERT INTO event_embeddings
                (event_id, embedding, model_name, model_version, metadata, created_at, updated_at)
                VALUES (:event_id, :embedding, :model_name, :model_version, :metadata, NOW(), NOW())
                ON DUPLICATE KEY UPDATE embedding = VALUES(embedding), metadata = VALUES(metadata), updated_at = NOW()
                """),
                {"event_id": event_id, "embedding": vec_str, "model_name": self._client.model_name, "model_version": "1.0", "metadata": metadata_json},
            )
            db.commit()

    def search_similar(
        self, query_embedding: list[float], limit: int = 10,
        session_id: str | None = None, filters: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        dim = self._client.dimension
        if len(query_embedding) != dim:
            raise ValueError(f"Query must be {dim} dimensions, got {len(query_embedding)}")
        vec_str = "[" + ",".join(str(x) for x in query_embedding) + "]"
        where_clauses = []
        if session_id:
            where_clauses.append("e.session_id = :session_id")
        if filters:
            for i, (key, value) in enumerate(filters.items()):
                param_name = f"filter_{i}"
                if key == "event_type":
                    where_clauses.append(f"e.event_type = :{param_name}")
                else:
                    where_clauses.append(f"JSON_UNQUOTE(JSON_EXTRACT(emb.metadata, '$.{key}')) = :{param_name}")
        where_clause = "WHERE " + " AND ".join(where_clauses) if where_clauses else ""
        query = f"""
            SELECT e.event_id, e.session_id, e.content, e.event_type, e.created_at,
                L2_DISTANCE(emb.embedding, :vec1) AS distance,
                1.0 / (1.0 + L2_DISTANCE(emb.embedding, :vec2)) AS similarity
            FROM conversation_events e
            JOIN event_embeddings emb ON e.event_id = emb.event_id
            {where_clause}
            ORDER BY distance ASC LIMIT :limit
        """
        from sqlalchemy import text
        params = {"vec1": vec_str, "vec2": vec_str, "limit": limit}
        if session_id:
            params["session_id"] = session_id
        if filters:
            for i, (key, value) in enumerate(filters.items()):
                params[f"filter_{i}"] = value
        with self._db() as db:
            result = db.execute(text(query), params)
            return [dict(row._mapping) for row in result.fetchall()]
