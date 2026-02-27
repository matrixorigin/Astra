"""Embedding service for semantic search.

Provider resolution order:
1. OpenAI-compatible API (from llm_models or tokens table) — best quality
2. Local sentence-transformers model — free, good quality, ~100ms/embed
3. Mock (hash-based) — deterministic but no semantic similarity

The DB schema uses vecf32(1536). Local models with smaller native dimensions
(e.g. 384 for all-MiniLM-L6-v2) are zero-padded to 1536. This is mathematically
safe: L2_DISTANCE on zero-padded vectors equals L2_DISTANCE on the original
dimensions because zeros contribute 0 to the sum of squares.
"""

import json
import os
from typing import Any

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)

# Providers whose API does not expose an embeddings endpoint.
_NO_EMBED_API = {"deepseek", "groq"}

# Module-level cache: load the local model once across all EmbeddingService instances.
_local_model_cache = None
_local_model_dim = 0


class EmbeddingService(DbConsumer):
    """Generate and manage text embeddings."""

    DIMENSION = 1536  # DB column width (vecf32(1536))

    def __init__(
        self, db_factory: DbFactory, provider: str = "openai", model: str = "text-embedding-3-small"
    ):
        super().__init__(db_factory)
        self.provider = provider
        self.model = model
        self._local_model = None
        self._local_dim = 0
        self._init_provider()
        logger.info("EmbeddingService: provider=%s, model=%s, dim=%d", self.provider, self.model, self.DIMENSION)

    # ------------------------------------------------------------------
    # Provider init
    # ------------------------------------------------------------------

    def _init_provider(self):
        if self.provider == "openai":
            self._try_openai_provider()

        # Fallback chain: openai failed or unavailable → local → mock
        if self.provider == "local":
            self._try_local_provider()

        if self.provider == "mock":
            logger.info("Using mock embeddings (hash-based, no semantic similarity)")

    def _try_openai_provider(self):
        """Try to initialize an OpenAI-compatible embedding client."""
        try:
            import openai
            from core.auth.encryption import decrypt_token
        except ImportError:
            if not os.getenv("PYTEST_CURRENT_TEST"):
                logger.warning("openai package not installed, trying local embeddings")
            self.provider = "local"
            return

        api_key, base_url, actual_provider = self._load_api_credentials()

        if not api_key or actual_provider in _NO_EMBED_API:
            self.provider = "local"
            return

        kwargs = {"api_key": api_key}
        if base_url:
            kwargs["base_url"] = base_url
        self.client = openai.OpenAI(**kwargs)

    def _load_api_credentials(self):
        """Load API key from llm_models (primary) or tokens table (legacy)."""
        from core.auth.encryption import decrypt_token

        if not self._db_factory:
            return None, None, None
        try:
            from sqlalchemy import text
            with self._db() as db:
                # Primary: llm_models table
                row = db.execute(
                    text("SELECT api_key_encrypted, provider, base_url FROM llm_models WHERE is_active=1 ORDER BY created_at LIMIT 1")
                ).first()
                if row:
                    return decrypt_token(row[0]) if row[0] else None, row[2], row[1]

                # Legacy: tokens table
                row = db.execute(
                    text("SELECT encrypted_value, provider, metadata FROM tokens WHERE type='llm' AND is_active=TRUE ORDER BY created_at DESC LIMIT 1")
                ).first()
                if row:
                    api_key = decrypt_token(row[0]) if row[0] else None
                    meta = row[2]
                    base_url = None
                    if meta:
                        try:
                            meta_dict = json.loads(meta) if isinstance(meta, str) else meta
                            base_url = meta_dict.get("base_url")
                        except Exception:
                            pass
                    return api_key, base_url, row[1]
        except Exception:
            pass
        return None, None, None

    def _try_local_provider(self):
        """Try to initialize sentence-transformers for local embeddings."""
        global _local_model_cache, _local_model_dim
        try:
            if _local_model_cache is None:
                from sentence_transformers import SentenceTransformer
                _local_model_cache = SentenceTransformer("all-MiniLM-L6-v2")
                _local_model_dim = _local_model_cache.get_sentence_embedding_dimension()
                logger.info("Loaded local model all-MiniLM-L6-v2 (dim=%d)", _local_model_dim)
            self._local_model = _local_model_cache
            self._local_dim = _local_model_dim
            self.model = "all-MiniLM-L6-v2"
        except ImportError:
            logger.info("sentence-transformers not installed, falling back to mock")
            self.provider = "mock"

    # ------------------------------------------------------------------
    # Embed
    # ------------------------------------------------------------------

    def embed_text(self, text: str) -> list[float]:
        """Generate embedding vector (always DIMENSION=1536)."""
        if self.provider == "openai":
            return self._embed_openai(text)
        elif self.provider == "local":
            return self._embed_local(text)
        else:
            return self._embed_mock(text)

    def _embed_openai(self, text: str) -> list[float]:
        try:
            response = self.client.embeddings.create(model=self.model, input=text, dimensions=self.DIMENSION)
            return response.data[0].embedding
        except Exception as e:
            logger.error("OpenAI embedding failed: %s", e)
            return self._embed_mock(text)

    def _embed_local(self, text: str) -> list[float]:
        """Encode with local model, zero-pad to DIMENSION.

        Zero-padding is safe for L2_DISTANCE: the padded zeros contribute 0 to
        the distance calculation, so ranking is identical to native-dim L2.
        """
        raw = self._local_model.encode(text).tolist()
        if len(raw) < self.DIMENSION:
            raw.extend([0.0] * (self.DIMENSION - len(raw)))
        return raw[: self.DIMENSION]

    def _embed_mock(self, text: str) -> list[float]:
        """Deterministic hash-based embedding (no semantic similarity)."""
        import hashlib
        hash_bytes = hashlib.sha256(text.encode()).digest()
        vector = []
        for i in range(0, len(hash_bytes), 2):
            val = (hash_bytes[i] * 256 + hash_bytes[i + 1]) / 65535.0
            vector.append(val * 2 - 1)
        while len(vector) < self.DIMENSION:
            vector.extend(vector[: self.DIMENSION - len(vector)])
        return vector[: self.DIMENSION]

    # ------------------------------------------------------------------
    # Store & search (event_embeddings table)
    # ------------------------------------------------------------------

    def store_embedding(self, event_id: str, embedding: list[float], metadata: dict[str, Any] | None = None):
        if len(embedding) != self.DIMENSION:
            raise ValueError(f"Embedding must be {self.DIMENSION} dimensions, got {len(embedding)}")
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
                {"event_id": event_id, "embedding": vec_str, "model_name": self.model, "model_version": "1.0", "metadata": metadata_json},
            )
            db.commit()

    def search_similar(
        self, query_embedding: list[float], limit: int = 10,
        session_id: str | None = None, filters: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        if len(query_embedding) != self.DIMENSION:
            raise ValueError(f"Query must be {self.DIMENSION} dimensions, got {len(query_embedding)}")
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
