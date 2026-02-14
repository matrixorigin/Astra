"""Embedding service for semantic search.

Provides text embedding generation and similarity search.
"""

import json
from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)


class EmbeddingService:
    """Generate and manage text embeddings."""

    DIMENSION = 1536  # OpenAI text-embedding-3-small dimension

    def __init__(
        self, db, provider: str = "openai", model: str = "text-embedding-3-small"
    ):
        """Initialize embedding service.

        Args:
            db: SQLAlchemy Session
            provider: Embedding provider (openai, mock)
            model: Model name
        """
        self.db = db
        self.provider = provider
        self.model = model
        self._init_provider()
        logger.info(
            f"EmbeddingService initialized: provider={provider}, model={model}, dim={self.DIMENSION}"
        )

    def _init_provider(self):
        """Initialize embedding provider."""
        if self.provider == "openai":
            try:
                import openai

                self.client = openai.OpenAI()
            except ImportError:
                logger.warning("OpenAI not available, falling back to mock")
                self.provider = "mock"

        if self.provider == "mock":
            logger.info("Using mock embeddings (hash-based)")

    def embed_text(self, text: str) -> list[float]:
        """Generate embedding for text.

        Args:
            text: Input text

        Returns:
            Embedding vector (1024 dimensions)
        """
        if self.provider == "openai":
            return self._embed_openai(text)
        else:
            return self._embed_mock(text)

    def _embed_openai(self, text: str) -> list[float]:
        """Generate OpenAI embedding."""
        try:
            response = self.client.embeddings.create(
                model=self.model,
                input=text,
                dimensions=self.DIMENSION,  # Request specific dimension
            )
            embedding: list[float] = response.data[0].embedding
            return embedding
        except Exception as e:
            logger.error(f"OpenAI embedding failed: {e}")
            return self._embed_mock(text)

    def _embed_mock(self, text: str) -> list[float]:
        """Generate mock embedding (deterministic hash-based)."""
        import hashlib

        hash_obj = hashlib.sha256(text.encode())
        hash_bytes = hash_obj.digest()

        # Convert to float vector
        vector = []
        for i in range(0, len(hash_bytes), 2):
            val = (hash_bytes[i] * 256 + hash_bytes[i + 1]) / 65535.0
            vector.append(val * 2 - 1)  # Normalize to [-1, 1]

        # Extend to DIMENSION
        while len(vector) < self.DIMENSION:
            vector.extend(vector[: self.DIMENSION - len(vector)])

        return vector[: self.DIMENSION]

    def store_embedding(
        self, event_id: str, embedding: list[float], metadata: dict[str, Any] | None = None
    ):
        """Store embedding in database.

        Args:
            event_id: Event identifier
            embedding: Embedding vector (must be 1024 dimensions)
            metadata: Optional metadata
        """
        if len(embedding) != self.DIMENSION:
            raise ValueError(f"Embedding must be {self.DIMENSION} dimensions, got {len(embedding)}")

        # Convert to MatrixOne vecf32 format: "[0.1, 0.2, ...]"
        vec_str = "[" + ",".join(str(x) for x in embedding) + "]"
        metadata_json = json.dumps(metadata or {})

        from sqlalchemy import text
        self.db.execute(
            text("""
            INSERT INTO event_embeddings
            (event_id, embedding, model_name, model_version, metadata, created_at, updated_at)
            VALUES (:event_id, :embedding, :model_name, :model_version, :metadata, NOW(), NOW())
            ON DUPLICATE KEY UPDATE
            embedding = VALUES(embedding),
            metadata = VALUES(metadata),
            updated_at = NOW()
        """),
            {"event_id": event_id, "embedding": vec_str, "model_name": self.model, "model_version": "1.0", "metadata": metadata_json},
        )
        self.db.commit()

    def search_similar(
        self,
        query_embedding: list[float],
        limit: int = 10,
        session_id: str | None = None,
        filters: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        """Search for similar events using L2_DISTANCE.

        Args:
            query_embedding: Query vector (1024 dimensions)
            limit: Max results
            session_id: Optional session filter
            filters: Optional metadata filters

        Returns:
            List of similar events with distances
        """
        if len(query_embedding) != self.DIMENSION:
            raise ValueError(
                f"Query embedding must be {self.DIMENSION} dimensions, got {len(query_embedding)}"
            )

        # Convert to MatrixOne vecf32 format
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
                    where_clauses.append(
                        f"JSON_UNQUOTE(JSON_EXTRACT(emb.metadata, '$.{key}')) = :{param_name}"
                    )

        where_clause = "WHERE " + " AND ".join(where_clauses) if where_clauses else ""

        # Use MatrixOne's native L2_DISTANCE function with vecf32
        query = f"""
            SELECT
                e.event_id,
                e.session_id,
                e.content,
                e.event_type,
                e.created_at,
                L2_DISTANCE(CAST(emb.embedding AS vecf32), CAST(:vec1 AS vecf32)) AS distance,
                1.0 / (1.0 + L2_DISTANCE(CAST(emb.embedding AS vecf32), CAST(:vec2 AS vecf32))) AS similarity
            FROM conversation_events e
            JOIN event_embeddings emb ON e.event_id = emb.event_id
            {where_clause}
            ORDER BY distance ASC
            LIMIT :limit
        """

        from sqlalchemy import text
        # Build params dict
        params_dict = {"vec1": vec_str, "vec2": vec_str, "limit": limit}
        
        # Add session_id and filters params
        if session_id:
            params_dict["session_id"] = session_id
        
        if filters:
            for i, (key, value) in enumerate(filters.items()):
                param_name = f"filter_{i}"
                params_dict[param_name] = value
        
        result = self.db.execute(text(query), params_dict)
        return [dict(row._mapping) for row in result.fetchall()]
