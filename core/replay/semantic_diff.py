"""Semantic Diff - Compare agent decisions and behaviors.

Provides high-level comparison of agent performance, not just data differences.
"""

from typing import Optional

from core.events.causal_chain import CausalChainManager
from core.events.event_reader import EventReader
from sdk.database import Database


class SemanticDiff:
    """Semantic difference analyzer for agent behaviors.
    
    Compares agent decisions, token usage, and execution paths
    rather than just raw data differences.
    """

    def __init__(self, db: Optional[Database] = None) -> None:
        """Initialize semantic diff analyzer.
        
        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()
        self.reader = EventReader(db)
        self.chain_mgr = CausalChainManager(db)

    def compare_sessions(
        self, session_id1: str, session_id2: str
    ) -> dict:
        """Compare two sessions semantically.
        
        Args:
            session_id1: First session ID
            session_id2: Second session ID
            
        Returns:
            dict: Semantic comparison results
        """
        # Get events for both sessions
        events1 = self.reader.get_session_events(session_id1)
        events2 = self.reader.get_session_events(session_id2)

        # Compare token usage
        token_diff = self._compare_token_usage(events1, events2)

        # Compare decision paths
        path_diff = self._compare_decision_paths(events1, events2)

        # Compare event types distribution
        type_diff = self._compare_event_types(events1, events2)

        # Compare response quality
        quality_diff = self._compare_quality(events1, events2)

        return {
            "session1": session_id1,
            "session2": session_id2,
            "token_usage": token_diff,
            "decision_paths": path_diff,
            "event_types": type_diff,
            "quality": quality_diff,
            "summary": self._generate_summary(token_diff, path_diff, type_diff),
        }

    def compare_checkpoints(
        self, checkpoint1: str, checkpoint2: str, session_id: str
    ) -> dict:
        """Compare agent behavior at two different checkpoints.
        
        Args:
            checkpoint1: First checkpoint name
            checkpoint2: Second checkpoint name
            session_id: Session to compare
            
        Returns:
            dict: Semantic comparison at two time points
        """
        # Query events at each checkpoint
        query1 = f"""
            SELECT * FROM conversation_events {{SNAPSHOT = '{checkpoint1}'}}
            WHERE session_id = %s
            ORDER BY created_at ASC
        """
        events1_rows = self.db.fetchall(query1, (session_id,))
        events1 = [self.reader._row_to_event(row) for row in events1_rows]

        query2 = f"""
            SELECT * FROM conversation_events {{SNAPSHOT = '{checkpoint2}'}}
            WHERE session_id = %s
            ORDER BY created_at ASC
        """
        events2_rows = self.db.fetchall(query2, (session_id,))
        events2 = [self.reader._row_to_event(row) for row in events2_rows]

        # Compare
        token_diff = self._compare_token_usage(events1, events2)
        path_diff = self._compare_decision_paths(events1, events2)

        return {
            "checkpoint1": checkpoint1,
            "checkpoint2": checkpoint2,
            "session_id": session_id,
            "token_usage": token_diff,
            "decision_paths": path_diff,
            "event_count_diff": len(events2) - len(events1),
        }

    def _compare_token_usage(self, events1: list, events2: list) -> dict:
        """Compare token usage between two event lists."""
        total1 = sum(e.token_usage.total for e in events1 if e.token_usage)
        total2 = sum(e.token_usage.total for e in events2 if e.token_usage)

        prompt1 = sum(e.token_usage.prompt for e in events1 if e.token_usage)
        prompt2 = sum(e.token_usage.prompt for e in events2 if e.token_usage)

        completion1 = sum(e.token_usage.completion for e in events1 if e.token_usage)
        completion2 = sum(e.token_usage.completion for e in events2 if e.token_usage)

        return {
            "total": {"session1": total1, "session2": total2, "diff": total2 - total1},
            "prompt": {"session1": prompt1, "session2": prompt2, "diff": prompt2 - prompt1},
            "completion": {
                "session1": completion1,
                "session2": completion2,
                "diff": completion2 - completion1,
            },
            "efficiency_change": (
                f"{((total2 - total1) / total1 * 100):.1f}%" if total1 > 0 else "N/A"
            ),
        }

    def _compare_decision_paths(self, events1: list, events2: list) -> dict:
        """Compare decision paths (causal chains)."""
        # Get unique causal chains
        chains1 = set(e.causal_chain_id for e in events1 if e.causal_chain_id)
        chains2 = set(e.causal_chain_id for e in events2 if e.causal_chain_id)

        # Count events per chain
        chain_lengths1 = {}
        for chain_id in chains1:
            chain_events = [e for e in events1 if e.causal_chain_id == chain_id]
            chain_lengths1[chain_id] = len(chain_events)

        chain_lengths2 = {}
        for chain_id in chains2:
            chain_events = [e for e in events2 if e.causal_chain_id == chain_id]
            chain_lengths2[chain_id] = len(chain_events)

        avg_length1 = (
            sum(chain_lengths1.values()) / len(chain_lengths1) if chain_lengths1 else 0
        )
        avg_length2 = (
            sum(chain_lengths2.values()) / len(chain_lengths2) if chain_lengths2 else 0
        )

        return {
            "chain_count": {
                "session1": len(chains1),
                "session2": len(chains2),
                "diff": len(chains2) - len(chains1),
            },
            "avg_chain_length": {
                "session1": avg_length1,
                "session2": avg_length2,
                "diff": avg_length2 - avg_length1,
            },
            "complexity_change": (
                "increased" if avg_length2 > avg_length1 else "decreased"
            ),
        }

    def _compare_event_types(self, events1: list, events2: list) -> dict:
        """Compare event type distribution."""
        from collections import Counter

        types1 = Counter(e.event_type for e in events1)
        types2 = Counter(e.event_type for e in events2)

        all_types = set(types1.keys()) | set(types2.keys())

        comparison = {}
        for event_type in all_types:
            count1 = types1.get(event_type, 0)
            count2 = types2.get(event_type, 0)
            comparison[event_type] = {
                "session1": count1,
                "session2": count2,
                "diff": count2 - count1,
            }

        return comparison

    def _compare_quality(self, events1: list, events2: list) -> dict:
        """Compare response quality scores."""
        scores1 = [e.quality_score for e in events1 if e.quality_score is not None]
        scores2 = [e.quality_score for e in events2 if e.quality_score is not None]

        avg1 = sum(scores1) / len(scores1) if scores1 else 0
        avg2 = sum(scores2) / len(scores2) if scores2 else 0

        return {
            "avg_quality": {
                "session1": avg1,
                "session2": avg2,
                "diff": avg2 - avg1,
            },
            "quality_change": "improved" if avg2 > avg1 else "degraded",
        }

    def _generate_summary(
        self, token_diff: dict, path_diff: dict, type_diff: dict
    ) -> str:
        """Generate human-readable summary."""
        token_change = token_diff["total"]["diff"]
        chain_change = path_diff["chain_count"]["diff"]

        summary_parts = []

        if token_change > 0:
            summary_parts.append(f"Used {token_change} more tokens")
        elif token_change < 0:
            summary_parts.append(f"Saved {abs(token_change)} tokens")

        if chain_change > 0:
            summary_parts.append(f"{chain_change} more decision chains")
        elif chain_change < 0:
            summary_parts.append(f"{abs(chain_change)} fewer decision chains")

        return "; ".join(summary_parts) if summary_parts else "No significant changes"
