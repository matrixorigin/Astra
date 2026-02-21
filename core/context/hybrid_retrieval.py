"""Hybrid retrieval using MatrixOne native vector + fulltext search.

Implements single-SQL hybrid search combining:
- Semantic similarity (vector L2 distance)
- Keyword matching (fulltext search)
- Temporal decay (recency scoring)
- Causal proximity (same chain bonus)
"""

import json
from typing import Any
from sqlalchemy.orm import Session
from sqlalchemy import text
from core.logging_config import get_logger

logger = get_logger(__name__)


class HybridRetriever:
    """MatrixOne-native hybrid retrieval for episodic and semantic memory."""
    
    def __init__(self, db: Session):
        self.db = db
    
    def retrieve_events(
        self,
        query_text: str,
        query_embedding: list[float],
        session_id: str,
        current_chain_id: str | None = None,
        limit: int = 20,
        weights: dict[str, float] | None = None,
    ) -> list[dict[str, Any]]:
        """Retrieve relevant events using hybrid search (vector + fulltext).
        
        Two-path approach:
        1. Vector search: semantic similarity with temporal/causal scoring
        2. Fulltext search: keyword matching
        Then rerank in Python combining both scores.
        
        Args:
            query_text: User query text for keyword matching
            query_embedding: Query embedding vector for semantic search
            session_id: Current session ID
            current_chain_id: Current causal chain ID for proximity bonus
            limit: Max results to return
            weights: Scoring weights (semantic, keyword, temporal, causal)
            
        Returns:
            List of events with relevance scores
        """
        if weights is None:
            weights = {
                "semantic": 0.35,
                "keyword": 0.25,
                "temporal": 0.20,
                "causal": 0.20,
            }
        
        embedding_str = "[" + ",".join(str(x) for x in query_embedding) + "]"
        events_by_id = {}
        
        # 1. Vector search (semantic + temporal + causal)
        try:
            vector_sql = text("""
                SELECT 
                    event_id,
                    session_id,
                    event_type,
                    content,
                    created_at,
                    causal_chain_id,
                    parent_event_id,
                    metadata,
                    (
                        :w_semantic * IFNULL(1.0 / (1.0 + l2_distance(embedding, :query_vec)), 0) +
                        :w_temporal * EXP(-TIMESTAMPDIFF(HOUR, created_at, NOW()) / 24.0) +
                        :w_causal * CASE 
                            WHEN causal_chain_id = :chain_id THEN 1.0 
                            ELSE 0.0 
                        END
                    ) AS vector_score
                FROM conversation_events
                WHERE session_id = :session_id AND embedding IS NOT NULL
                ORDER BY vector_score DESC
                LIMIT :limit
            """)
            
            result = self.db.execute(
                vector_sql,
                {
                    "query_vec": embedding_str,
                    "session_id": session_id,
                    "chain_id": current_chain_id or "",
                    "limit": limit,
                    "w_semantic": weights["semantic"],
                    "w_temporal": weights["temporal"],
                    "w_causal": weights["causal"],
                }
            )
            
            for row in result:
                events_by_id[row.event_id] = {
                    "event_id": row.event_id,
                    "session_id": row.session_id,
                    "event_type": row.event_type,
                    "content": row.content,
                    "created_at": row.created_at.isoformat() if row.created_at else None,
                    "causal_chain_id": row.causal_chain_id,
                    "parent_event_id": row.parent_event_id,
                    "metadata": row.metadata,
                    "vector_score": float(row.vector_score),
                    "keyword_score": 0.0,
                }
        except Exception as e:
            logger.warning(f"Vector search failed: {e}")
        
        # 2. Fulltext search (keyword matching with session_id filter in WHERE)
        try:
            fulltext_sql = text("""
                SELECT 
                    event_id,
                    session_id,
                    event_type,
                    content,
                    created_at,
                    causal_chain_id,
                    parent_event_id,
                    metadata
                FROM conversation_events
                WHERE MATCH(content, session_id) AGAINST(:query_text IN BOOLEAN MODE)
                    AND session_id = :session_id
                LIMIT :limit
            """)
            
            result = self.db.execute(
                fulltext_sql,
                {
                    "query_text": query_text,
                    "session_id": session_id,
                    "limit": limit,
                }
            )
            
            for row in result:
                if row.event_id in events_by_id:
                    # Already in vector results, add keyword score
                    events_by_id[row.event_id]["keyword_score"] = weights["keyword"]
                else:
                    # New from fulltext
                    events_by_id[row.event_id] = {
                        "event_id": row.event_id,
                        "session_id": row.session_id,
                        "event_type": row.event_type,
                        "content": row.content,
                        "created_at": row.created_at.isoformat() if row.created_at else None,
                        "causal_chain_id": row.causal_chain_id,
                        "parent_event_id": row.parent_event_id,
                        "metadata": row.metadata,
                        "vector_score": 0.0,
                        "keyword_score": weights["keyword"],
                    }
        except Exception as e:
            logger.warning(f"Fulltext search failed: {e}")
        
        # 3. Rerank in Python
        events = list(events_by_id.values())
        for event in events:
            event["relevance_score"] = event["vector_score"] + event["keyword_score"]
        
        events.sort(key=lambda x: x["relevance_score"], reverse=True)
        events = events[:limit]
        
        # Remove internal scores from output
        for event in events:
            del event["vector_score"]
            del event["keyword_score"]
        
        logger.info(f"Hybrid retrieval: {len(events)} events, top score: {events[0]['relevance_score']:.3f}" if events else "No events found")
        return events
    
    def retrieve_knowledge(
        self,
        query_text: str,
        query_embedding: list[float],
        user_id: str,
        limit: int = 10,
        confidence_threshold: float = 0.3,
        weights: dict[str, float] | None = None,
    ) -> list[dict[str, Any]]:
        """Retrieve relevant knowledge entries using hybrid search.
        
        Args:
            query_text: User query text for keyword matching
            query_embedding: Query embedding vector for semantic search
            user_id: User ID
            limit: Max results to return
            confidence_threshold: Min confidence to include (default 0.3)
            weights: Scoring weights (semantic, keyword, confidence)
            
        Returns:
            List of knowledge entries with relevance scores
        """
        if weights is None:
            weights = {
                "semantic": 0.5,
                "keyword": 0.3,
                "confidence": 0.2,
            }
        
        # Validate weights dict
        required_keys = {"semantic", "keyword", "confidence"}
        if not required_keys.issubset(weights.keys()):
            logger.error(f"Invalid weights dict, missing keys: {required_keys - weights.keys()}")
            return []
        
        # Convert embedding to string format for SQL
        # Note: This is safe as query_embedding is a list of floats from our own embedding service
        embedding_str = "[" + ",".join(str(float(x)) for x in query_embedding) + "]"
        
        # Build hybrid search query for semantic memory
        sql = text("""
            SELECT 
                entry_id,
                category,
                key_name,
                value,
                confidence,
                trust_tier,
                created_at,
                last_validated_at,
                (
                    :w_semantic * IFNULL(1.0 / (1.0 + l2_distance(embedding, :query_vec)), 0) +
                    :w_keyword * COALESCE(
                        MATCH(value) AGAINST(:query_text IN BOOLEAN MODE), 
                        0
                    ) +
                    :w_confidence * confidence
                ) AS relevance_score
            FROM knowledge_entries
            WHERE user_id = :user_id
                AND confidence > :threshold
                AND embedding IS NOT NULL
            ORDER BY relevance_score DESC
            LIMIT :limit
        """)
        
        entries = []
        try:
            result = self.db.execute(
                sql,
                {
                    "query_vec": embedding_str,
                    "query_text": query_text,
                    "user_id": user_id,
                    "threshold": confidence_threshold,
                    "limit": limit,
                    "w_semantic": weights["semantic"],
                    "w_keyword": weights["keyword"],
                    "w_confidence": weights["confidence"],
                }
            )
            
            for row in result:
                entries.append({
                    "entry_id": row.entry_id,
                    "category": row.category,
                    "key_name": row.key_name,
                    "value": row.value,
                    "confidence": float(row.confidence),
                    "trust_tier": row.trust_tier,
                    "created_at": row.created_at.isoformat() if row.created_at else None,
                    "last_validated_at": row.last_validated_at.isoformat() if row.last_validated_at else None,
                    "relevance_score": float(row.relevance_score),
                })
            
            logger.info(f"Knowledge retrieval: {len(entries)} entries, top score: {entries[0]['relevance_score']:.3f}" if entries else "No entries found")
        except Exception as e:
            logger.error(f"Knowledge retrieval failed: {e}")

        # Access tracking — outside try/except so retrieval errors don't lose results
        if entries:
            from core.context.knowledge import update_access_tracking
            update_access_tracking(self.db, [e["entry_id"] for e in entries])

        return entries
