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
    """MatrixOne-native hybrid retrieval for episodic memory."""
    
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
        """Retrieve relevant events using hybrid search.
        
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
        
        # Convert embedding to string format for SQL
        embedding_str = "[" + ",".join(str(x) for x in query_embedding) + "]"
        
        # Build hybrid search query
        sql = text("""
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
                    :w_semantic * (1.0 / (1.0 + l2_distance(embedding, :query_vec))) +
                    :w_keyword * COALESCE(
                        MATCH(content) AGAINST(:query_text IN NATURAL LANGUAGE MODE), 
                        0
                    ) +
                    :w_temporal * EXP(-TIMESTAMPDIFF(HOUR, created_at, NOW()) / 24.0) +
                    :w_causal * CASE 
                        WHEN causal_chain_id = :chain_id THEN 1.0 
                        ELSE 0.0 
                    END
                ) AS relevance_score
            FROM conversation_events
            WHERE session_id = :session_id
                AND embedding IS NOT NULL
            ORDER BY relevance_score DESC
            LIMIT :limit
        """)
        
        try:
            result = self.db.execute(
                sql,
                {
                    "query_vec": embedding_str,
                    "query_text": query_text,
                    "session_id": session_id,
                    "chain_id": current_chain_id or "",
                    "limit": limit,
                    "w_semantic": weights["semantic"],
                    "w_keyword": weights["keyword"],
                    "w_temporal": weights["temporal"],
                    "w_causal": weights["causal"],
                }
            )
            
            events = []
            for row in result:
                events.append({
                    "event_id": row.event_id,
                    "session_id": row.session_id,
                    "event_type": row.event_type,
                    "content": row.content,
                    "created_at": row.created_at.isoformat() if row.created_at else None,
                    "causal_chain_id": row.causal_chain_id,
                    "parent_event_id": row.parent_event_id,
                    "metadata": row.metadata,
                    "relevance_score": float(row.relevance_score),
                })
            
            logger.info(f"Hybrid retrieval: {len(events)} events, top score: {events[0]['relevance_score']:.3f}" if events else "No events found")
            return events
        except Exception as e:
            logger.error(f"Hybrid retrieval failed: {e}")
            return []
