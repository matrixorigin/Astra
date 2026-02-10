"""Embedding service for semantic search.

Provides text embedding generation and similarity search.
"""

from typing import List, Dict, Any, Optional
import json

from sdk import Database
from core.logging_config import get_logger

logger = get_logger(__name__)


class EmbeddingService:
    """Generate and manage text embeddings."""
    
    DIMENSION = 1024  # Fixed dimension for VECF32(1024)
    
    def __init__(self, db: Database, provider: str = "openai", model: str = "text-embedding-3-small"):
        """Initialize embedding service.
        
        Args:
            db: Database connection
            provider: Embedding provider (openai, mock)
            model: Model name
        """
        self.db = db
        self.provider = provider
        self.model = model
        self._init_provider()
        logger.info(f"EmbeddingService initialized: provider={provider}, model={model}, dim={self.DIMENSION}")
    
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
    
    def embed_text(self, text: str) -> List[float]:
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
    
    def _embed_openai(self, text: str) -> List[float]:
        """Generate OpenAI embedding."""
        try:
            response = self.client.embeddings.create(
                model=self.model,
                input=text,
                dimensions=self.DIMENSION  # Request specific dimension
            )
            return response.data[0].embedding
        except Exception as e:
            logger.error(f"OpenAI embedding failed: {e}")
            return self._embed_mock(text)
    
    def _embed_mock(self, text: str) -> List[float]:
        """Generate mock embedding (deterministic hash-based)."""
        import hashlib
        
        hash_obj = hashlib.sha256(text.encode())
        hash_bytes = hash_obj.digest()
        
        # Convert to float vector
        vector = []
        for i in range(0, len(hash_bytes), 2):
            val = (hash_bytes[i] * 256 + hash_bytes[i+1]) / 65535.0
            vector.append(val * 2 - 1)  # Normalize to [-1, 1]
        
        # Extend to DIMENSION
        while len(vector) < self.DIMENSION:
            vector.extend(vector[:self.DIMENSION - len(vector)])
        
        return vector[:self.DIMENSION]
    
    def store_embedding(
        self,
        event_id: str,
        embedding: List[float],
        metadata: Optional[Dict[str, Any]] = None
    ):
        """Store embedding in database.
        
        Args:
            event_id: Event identifier
            embedding: Embedding vector (must be 1024 dimensions)
            metadata: Optional metadata
        """
        if len(embedding) != self.DIMENSION:
            raise ValueError(f"Embedding must be {self.DIMENSION} dimensions, got {len(embedding)}")
        
        # Convert to MatrixOne vector format: "[0.1, 0.2, ...]"
        vec_str = "[" + ",".join(str(x) for x in embedding) + "]"
        metadata_json = json.dumps(metadata or {})
        
        self.db.execute("""
            INSERT INTO event_embeddings 
            (event_id, embedding, model_name, model_version, metadata)
            VALUES (%s, %s, %s, %s, %s)
            ON DUPLICATE KEY UPDATE
                embedding = VALUES(embedding),
                model_name = VALUES(model_name),
                model_version = VALUES(model_version),
                metadata = VALUES(metadata),
                updated_at = CURRENT_TIMESTAMP
        """, (event_id, vec_str, self.model, "1.0", metadata_json))
    
    def search_similar(
        self,
        query_embedding: List[float],
        limit: int = 10,
        session_id: Optional[str] = None,
        filters: Optional[Dict[str, Any]] = None
    ) -> List[Dict[str, Any]]:
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
            raise ValueError(f"Query embedding must be {self.DIMENSION} dimensions, got {len(query_embedding)}")
        
        # Convert to MatrixOne vector format
        vec_str = "[" + ",".join(str(x) for x in query_embedding) + "]"
        
        where_clauses = []
        params = []
        
        if session_id:
            where_clauses.append("e.session_id = %s")
            params.append(session_id)
        
        if filters:
            for key, value in filters.items():
                if key == 'event_type':
                    where_clauses.append("e.event_type = %s")
                    params.append(value)
                else:
                    where_clauses.append(f"JSON_UNQUOTE(JSON_EXTRACT(emb.metadata, '$.{key}')) = %s")
                    params.append(value)
        
        where_clause = "WHERE " + " AND ".join(where_clauses) if where_clauses else ""
        
        # Use MatrixOne's native L2_DISTANCE function
        query = f"""
            SELECT 
                e.event_id,
                e.session_id,
                e.content,
                e.event_type,
                e.created_at,
                L2_DISTANCE(emb.embedding, %s) AS distance,
                1.0 / (1.0 + L2_DISTANCE(emb.embedding, %s)) AS similarity
            FROM conversation_events e
            JOIN event_embeddings emb ON e.event_id = emb.event_id
            {where_clause}
            ORDER BY distance ASC
            LIMIT %s
        """
        
        params = [vec_str, vec_str] + params + [limit]
        return self.db.fetchall(query, params)
