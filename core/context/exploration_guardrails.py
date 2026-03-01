"""Exploration guardrails with dynamic thresholds.

P0 Critical: SQL optimization with COALESCE fallback and recommended indexes.
Design ref: context-window-management.md §3 Dynamic Exploration Thresholds
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from sqlalchemy.orm import Session


# Base thresholds per agent type
EXPLORATION_THRESHOLDS = {
    "dev-agent": {"tier1": 4, "tier2": 7, "tier3": 12},      # Code exploration is common
    "data-analyst": {"tier1": 6, "tier2": 10, "tier3": 15},  # Data exploration is expected
    "chat-agent": {"tier1": 2, "tier2": 4, "tier3": 6},      # Exploration is rare
}


def get_dynamic_thresholds(agent_type: str, session_id: str, db: Session | None = None) -> dict[str, int]:
    """
    Get dynamic exploration thresholds based on learned satisfaction data.
    
    P0 SQL Optimization:
    - COALESCE(AVG(satisfaction_score), 0.7) for fallback when no data
    - Recommended indexes for performance
    
    Uses LOW_SATISFACTION signal from SelfImprovingSelector:
    - If exploration sessions have low satisfaction → lower thresholds
    - If exploration sessions have high satisfaction → raise thresholds
    
    Args:
        agent_type: Type of agent (dev-agent, data-analyst, chat-agent)
        session_id: Current session ID
        db: Database session (optional, for testing can be None)
        
    Returns:
        Dictionary with tier1, tier2, tier3 thresholds
        
    Example:
        >>> thresholds = get_dynamic_thresholds("dev-agent", "session_123")
        >>> thresholds
        {'tier1': 4, 'tier2': 7, 'tier3': 12}
    """
    base = EXPLORATION_THRESHOLDS.get(agent_type, EXPLORATION_THRESHOLDS["dev-agent"])
    
    # If no DB connection, return base thresholds
    if db is None:
        return base
    
    # P0: Query with COALESCE fallback + recommended indexes
    # Recommended indexes (create via migration):
    # CREATE INDEX idx_etp_session_tools ON edge_tool_patterns(session_id, tool_call_count);
    # CREATE INDEX idx_sse_agent_created ON skill_selection_events(agent_type, created_at);
    
    query = """
        SELECT COALESCE(AVG(satisfaction_score), 0.7) as avg_satisfaction
        FROM edge_tool_patterns etp
        JOIN skill_selection_events sse ON etp.session_id = sse.session_id
        WHERE etp.tool_call_count > 5
          AND sse.agent_type = :agent_type
          AND sse.created_at > NOW() - INTERVAL 30 DAY
    """
    
    try:
        result = db.execute(query, {"agent_type": agent_type}).fetchone()
        avg_satisfaction = result[0] if result else 0.7  # COALESCE ensures never NULL
        
        # Adjust thresholds based on satisfaction
        if avg_satisfaction < 0.6:
            # Low satisfaction → lower thresholds (intervene earlier)
            return {k: max(2, int(v * 0.7)) for k, v in base.items()}
        elif avg_satisfaction > 0.8:
            # High satisfaction → raise thresholds (allow more exploration)
            return {k: int(v * 1.3) for k, v in base.items()}
        else:
            return base
    except Exception:
        # If query fails, return base thresholds (safe fallback)
        return base


# SQL migration for recommended indexes
SQL_MIGRATION = """
-- P0: Recommended indexes for get_dynamic_thresholds performance

-- Index for edge_tool_patterns filtering
CREATE INDEX IF NOT EXISTS idx_etp_session_tools 
ON edge_tool_patterns(session_id, tool_call_count);

-- Index for skill_selection_events filtering
CREATE INDEX IF NOT EXISTS idx_sse_agent_created 
ON skill_selection_events(agent_type, created_at);
"""
