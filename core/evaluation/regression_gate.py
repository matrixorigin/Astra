"""Unified regression gate for prompt/skill/config changes.

Extends replay gating from selector to all versioned inputs.
"""

from datetime import datetime, timezone
from typing import Any, Optional
from enum import Enum

from sqlalchemy.orm import Session
from sqlalchemy import text
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.sandbox import Sandbox
from api.services.replay_service import ReplayService

logger = get_logger(__name__)


class ChangeType(str, Enum):
    """Type of change being validated"""
    PROMPT = "prompt"
    SKILL = "skill"
    CONFIG = "config"
    SELECTOR = "selector"
    CONTEXT_BUDGET = "context_budget"
    KNOWLEDGE = "knowledge"


class RegressionGate:
    """Unified regression gate for all versioned inputs.
    
    Validates changes don't degrade quality on golden sessions:
    1. Load golden sessions (high quality_score)
    2. Create snapshot + sandbox
    3. Apply change to sandbox
    4. Replay golden sessions with change
    5. Compute metrics (error rate, score delta, latency, tokens)
    6. Pass/fail decision
    7. Record gate result with lineage
    """
    
    def __init__(self, db: Session, account: str = "sys"):
        if not isinstance(db, Session):
            raise TypeError("db must be a SQLAlchemy Session")
        
        self.db = db
        self.account = account
        self.sandbox = Sandbox(db=db, account=account)
        self.replay_service = ReplayService(db)
    
    def validate_change(
        self,
        change_type: ChangeType,
        change_id: str,
        change_content: dict[str, Any],
        golden_session_count: int = 50,
        error_rate_threshold: float = 0.05,
        score_regression_threshold: float = -0.1,
    ) -> dict[str, Any]:
        """Validate change against golden sessions.
        
        Args:
            change_type: Type of change (prompt/skill/config/selector)
            change_id: Change identifier (e.g., "code_review@v3")
            change_content: Change content (prompt text, skill def, config values)
            golden_session_count: Number of golden sessions to test
            error_rate_threshold: Max allowed error rate (default 5%)
            score_regression_threshold: Max allowed score drop (default -0.1)
            
        Returns:
            Gate result dict with verdict, metrics, and lineage
        """
        gate_id = str(uuid7())
        sandbox_name = f"gate_{gate_id[:8]}"
        
        try:
            # 1. Load golden sessions
            golden_sessions = self._get_golden_sessions(golden_session_count)
            if not golden_sessions:
                logger.warning("No golden sessions found, skipping gate validation")
                return self._build_result(
                    gate_id=gate_id,
                    change_type=change_type,
                    change_id=change_id,
                    verdict="skip",
                    reason="no_golden_sessions_available",
                    sessions_tested=0,
                )
            
            # 2. Create snapshot + sandbox
            snapshot_id = self._create_snapshot()
            self.sandbox.create(sandbox_name, description=f"Gate {gate_id}", created_by="system")
            
            # 3. Apply change to sandbox
            self._apply_change_to_sandbox(
                sandbox_name=sandbox_name,
                change_type=change_type,
                change_id=change_id,
                change_content=change_content,
            )
            
            # 4. Replay golden sessions
            replay_results = []
            for session in golden_sessions:
                result = self.replay_service.replay_session(
                    session_id=session["session_id"],
                    user_id=session["user_id"],
                    sandbox_name=sandbox_name,
                    mock_mode=True,
                )
                replay_results.append({
                    "session_id": session["session_id"],
                    "original_score": session["avg_score"],
                    "replay_status": result["status"],
                    "events_replayed": result["events_replayed"],
                    "successful": result["result"]["successful"],
                    "failed": result["result"]["failed"],
                })
            
            # 5. Compute metrics
            metrics = self._compute_metrics(golden_sessions, replay_results)
            
            # 6. Pass/fail decision
            verdict, reason = self._make_decision(
                metrics=metrics,
                error_rate_threshold=error_rate_threshold,
                score_regression_threshold=score_regression_threshold,
            )
            
            # 7. Record gate result
            gate_result = self._build_result(
                gate_id=gate_id,
                change_type=change_type,
                change_id=change_id,
                verdict=verdict,
                reason=reason,
                sessions_tested=len(golden_sessions),
                snapshot_id=snapshot_id,
                metrics=metrics,
                replay_results=replay_results,
            )
            
            self._record_gate_result(gate_result)
            
            return gate_result
            
        finally:
            # Cleanup sandbox
            try:
                self.sandbox.delete(sandbox_name)
            except Exception as e:
                logger.warning(f"Failed to cleanup sandbox {sandbox_name}: {e}")
    
    def _get_golden_sessions(self, limit: int) -> list[dict[str, Any]]:
        """Get golden sessions with high quality scores.
        
        Selection criteria:
        - quality_score >= 4.0
        - training_eligible = TRUE
        - Multi-turn (event_count >= 3)
        - Recent (last 30 days)
        """
        result = self.db.execute(text("""
            SELECT 
                session_id,
                user_id,
                AVG(quality_score) as avg_score,
                COUNT(*) as event_count
            FROM conversation_events
            WHERE quality_score >= 4.0
              AND training_eligible = TRUE
              AND created_at > DATE_SUB(NOW(), INTERVAL 30 DAY)
            GROUP BY session_id, user_id
            HAVING event_count >= 3
            ORDER BY avg_score DESC
            LIMIT :limit
        """), {"limit": limit})
        
        return [
            {
                "session_id": row[0],
                "user_id": row[1],
                "avg_score": float(row[2]),
                "event_count": int(row[3]),
            }
            for row in result
        ]
    
    def _create_snapshot(self) -> str:
        """Create snapshot of current production state."""
        snapshot_id = f"snapshot_{datetime.now(timezone.utc).strftime('%Y%m%d_%H%M%S')}"
        # Snapshot creation is implicit in MatrixOne - just record the timestamp
        return snapshot_id
    
    def _apply_change_to_sandbox(
        self,
        sandbox_name: str,
        change_type: ChangeType,
        change_id: str,
        change_content: dict[str, Any],
    ):
        """Apply change to sandbox environment."""
        try:
            if change_type == ChangeType.PROMPT:
                # Update prompt template in sandbox
                self.db.execute(text(f"""
                    UPDATE {sandbox_name}.prompt_templates 
                    SET content = :content, updated_at = NOW()
                    WHERE template_id = :template_id
                """), {
                    "content": change_content.get("content", ""),
                    "template_id": change_content.get("template_id", change_id),
                })
            
            elif change_type == ChangeType.SKILL:
                skill_definition = change_content.get("definition")
                if skill_definition is None:
                    skill_definition = change_content.get("skill_definition", {})
                self.db.execute(text(f"""
                    INSERT INTO {sandbox_name}.skills_registry 
                    (skill_id, skill_name, version, description, skill_definition, is_active, created_at, updated_at)
                    VALUES (:skill_id, :skill_name, :version, :description, :definition, 1, NOW(), NOW())
                    ON DUPLICATE KEY UPDATE
                    skill_definition = :definition,
                    version = :version,
                    description = :description,
                    is_active = 1,
                    updated_at = NOW()
                """), {
                    "skill_id": change_id,
                    "skill_name": change_content.get("skill_name") or change_content.get("name", change_id),
                    "version": change_content.get("version", "1.0.0"),
                    "description": change_content.get("description", ""),
                    "definition": skill_definition,
                })
            
            elif change_type == ChangeType.CONFIG:
                # Update config in sandbox
                self.db.execute(text(f"""
                    UPDATE {sandbox_name}.configs 
                    SET value = :value, updated_at = NOW()
                    WHERE key_name = :key_name
                """), {
                    "key_name": change_content.get("key", change_id),
                    "value": change_content.get("value", ""),
                })
            
            elif change_type == ChangeType.SELECTOR:
                # Update selector config in sandbox
                self.db.execute(text(f"""
                    UPDATE {sandbox_name}.configs 
                    SET value = :value, updated_at = NOW()
                    WHERE key_name = 'selector_config'
                """), {
                    "value": str(change_content),
                })

            elif change_type == ChangeType.CONTEXT_BUDGET:
                # Update context budget ratios in sandbox
                self.db.execute(text(f"""
                    INSERT INTO {sandbox_name}.configs (key_name, value, updated_at)
                    VALUES ('context_budget_ratios', :value, NOW())
                    ON DUPLICATE KEY UPDATE value = :value, updated_at = NOW()
                """), {
                    "value": str(change_content),
                })

            elif change_type == ChangeType.KNOWLEDGE:
                # Apply knowledge change (quarantine/restore) in sandbox
                entry_id = change_content.get("entry_id")
                if not entry_id:
                    raise ValueError("KNOWLEDGE change requires entry_id")
                action = change_content.get("action", "quarantine")
                if action == "quarantine":
                    self.db.execute(text(f"""
                        UPDATE {sandbox_name}.sk_knowledge_entries
                        SET confidence = 0.0
                        WHERE entry_id = :entry_id
                    """), {"entry_id": entry_id})
                elif action == "restore":
                    self.db.execute(text(f"""
                        UPDATE {sandbox_name}.sk_knowledge_entries
                        SET confidence = :confidence
                        WHERE entry_id = :entry_id
                    """), {
                        "entry_id": entry_id,
                        "confidence": change_content.get("confidence", 0.8),
                    })
            
            self.db.commit()
            logger.info(f"Applied {change_type} change {change_id} to sandbox {sandbox_name}")
            
        except Exception as e:
            logger.error(f"Failed to apply change to sandbox: {e}")
            self.db.rollback()
            raise
    
    def _compute_metrics(
        self,
        golden_sessions: list[dict[str, Any]],
        replay_results: list[dict[str, Any]],
    ) -> dict[str, Any]:
        """Compute gate metrics from replay results."""
        total = len(replay_results)
        if total == 0:
            return {
                "error_rate": 0.0,
                "score_delta": 0.0,
                "avg_original_score": 0.0,
                "avg_replay_score": 0.0,
                "total_sessions": 0,
                "failed_sessions": 0,
            }
        
        # Error rate: failed replays / total
        failed = sum(1 for r in replay_results if r["replay_status"] != "completed")
        error_rate = failed / total
        
        # Score delta: Compare original vs replay quality
        # Use success rate as proxy for quality score
        # Successful replay = maintains quality, failed replay = quality degraded
        avg_original_score = sum(s["avg_score"] for s in golden_sessions) / total
        
        # Calculate replay score based on success/failure
        # Successful replays maintain original score
        # Failed replays get score of 0
        replay_scores = []
        for i, result in enumerate(replay_results):
            if result["replay_status"] == "completed" and result["failed"] == 0:
                # Successful replay maintains original quality
                replay_scores.append(golden_sessions[i]["avg_score"])
            else:
                # Failed replay = quality degraded to 0
                replay_scores.append(0.0)
        
        avg_replay_score = sum(replay_scores) / total if replay_scores else 0.0
        score_delta = avg_replay_score - avg_original_score
        
        return {
            "error_rate": error_rate,
            "score_delta": score_delta,
            "avg_original_score": avg_original_score,
            "avg_replay_score": avg_replay_score,
            "total_sessions": total,
            "failed_sessions": failed,
        }
    
    def _make_decision(
        self,
        metrics: dict[str, Any],
        error_rate_threshold: float,
        score_regression_threshold: float,
    ) -> tuple[str, str]:
        """Make pass/fail decision based on metrics.
        
        Returns:
            (verdict, reason) tuple
        """
        if metrics["error_rate"] > error_rate_threshold:
            return "fail", f"error_rate {metrics['error_rate']:.2%} > threshold {error_rate_threshold:.2%}"
        
        if metrics["score_delta"] < score_regression_threshold:
            return "fail", f"score_delta {metrics['score_delta']:.2f} < threshold {score_regression_threshold:.2f}"
        
        return "pass", "all_metrics_within_threshold"
    
    def _build_result(
        self,
        gate_id: str,
        change_type: ChangeType,
        change_id: str,
        verdict: str,
        reason: str,
        sessions_tested: int,
        snapshot_id: Optional[str] = None,
        metrics: Optional[dict[str, Any]] = None,
        replay_results: Optional[list[dict[str, Any]]] = None,
    ) -> dict[str, Any]:
        """Build gate result dict."""
        return {
            "gate_id": gate_id,
            "change_type": change_type.value,
            "change_id": change_id,
            "verdict": verdict,
            "reason": reason,
            "sessions_tested": sessions_tested,
            "snapshot_id": snapshot_id,
            "metrics": metrics or {},
            "replay_results": replay_results or [],
            "created_at": datetime.now(timezone.utc).isoformat(),
        }
    
    def _record_gate_result(self, gate_result: dict[str, Any]):
        """Record gate result to database with error handling."""
        try:
            # Convert ISO datetime string to datetime object
            created_at_str = gate_result["created_at"]
            created_at = datetime.fromisoformat(created_at_str.replace('+00:00', ''))
            
            self.db.execute(text("""
                INSERT INTO gate_results (
                    gate_id, change_type, change_id,
                    snapshot_used, sessions_tested,
                    error_rate, score_delta, passed,
                    metrics, created_at
                ) VALUES (
                    :gate_id, :change_type, :change_id,
                    :snapshot_id, :sessions_tested,
                    :error_rate, :score_delta, :passed,
                    :metrics, :created_at
                )
            """), {
                "gate_id": gate_result["gate_id"],
                "change_type": gate_result["change_type"],
                "change_id": gate_result["change_id"],
                "snapshot_id": gate_result.get("snapshot_id"),
                "sessions_tested": gate_result["sessions_tested"],
                "error_rate": gate_result["metrics"].get("error_rate", 0.0),
                "score_delta": gate_result["metrics"].get("score_delta", 0.0),
                "passed": gate_result["verdict"] == "pass",
                "metrics": str(gate_result["metrics"]),
                "created_at": created_at,
            })
            self.db.commit()
        except Exception as e:
            logger.error(f"Failed to record gate result: {e}")
            self.db.rollback()
            raise
    
    def get_gate_history(self, limit: int = 10) -> list[dict[str, Any]]:
        """Get gate validation history."""
        result = self.db.execute(text("""
            SELECT 
                gate_id, change_type, change_id,
                snapshot_used, sessions_tested,
                error_rate, score_delta, passed,
                metrics, created_at
            FROM gate_results
            ORDER BY created_at DESC
            LIMIT :limit
        """), {"limit": limit})
        
        return [
            {
                "gate_id": row[0],
                "change_type": row[1],
                "change_id": row[2],
                "snapshot_used": row[3],
                "sessions_tested": row[4],
                "error_rate": float(row[5]),
                "score_delta": float(row[6]),
                "passed": bool(row[7]),
                "metrics": row[8],
                "created_at": row[9].isoformat() if row[9] else None,
            }
            for row in result
        ]
