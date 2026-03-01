"""Session evaluation skill - enables agent self-assessment."""

from pydantic import Field
from core.skills.base import Skill, SkillInput, SkillOutput
from sqlalchemy import text
import json


class EvaluateSessionInput(SkillInput):
    """Input for session evaluation."""
    session_id: str = Field(..., description="Session ID to evaluate")
    include_details: bool = Field(default=False, description="Include detailed breakdown")


class EvaluateSessionOutput(SkillOutput):
    """Output from session evaluation."""
    success: bool = True
    session_id: str | None = None
    total_events: int = 0
    user_queries: int = 0
    llm_calls: int = 0
    tokens: dict = Field(default_factory=dict)
    skills: dict = Field(default_factory=dict)
    assessment: dict = Field(default_factory=dict)
    event_breakdown: list[dict] | None = None


class EvaluateSessionSkill(Skill[EvaluateSessionInput, EvaluateSessionOutput]):
    """Evaluate agent performance in a session."""
    
    name = "evaluate_session"
    description = "Evaluate agent performance metrics for a session"
    version = "1.0.0"
    
    def execute(self, input_data: EvaluateSessionInput) -> EvaluateSessionOutput:
        """Evaluate session performance."""
        from api.database import SessionLocal
        
        db = SessionLocal()
        try:
            events = db.execute(text("""
                SELECT event_type, token_usage, llm_model_used, skill_name, content
                FROM agent_events
                WHERE session_id = :session_id
                ORDER BY created_at
            """), {"session_id": input_data.session_id}).fetchall()
            
            if not events:
                return EvaluateSessionOutput(
                    success=False,
                    error=f"No events found for session {input_data.session_id}"
                )
            
            metrics = self._calculate_metrics(events, input_data.session_id)
            
            if input_data.include_details:
                metrics["event_breakdown"] = self._get_event_breakdown(events)
            
            metrics["assessment"] = self._generate_assessment(metrics)
            
            return EvaluateSessionOutput(**metrics)
            
        finally:
            db.close()
    
    def _calculate_metrics(self, events, session_id: str) -> dict:
        """Calculate performance metrics from events."""
        total_prompt = 0
        total_completion = 0
        llm_calls = 0
        user_queries = 0
        skills_used = []
        
        for evt in events:
            if evt[0] == "user_query":
                user_queries += 1
            
            if evt[1]:
                try:
                    usage = json.loads(evt[1]) if isinstance(evt[1], str) else evt[1]
                    total_prompt += usage.get('prompt', usage.get('prompt_tokens', 0))
                    total_completion += usage.get('completion', usage.get('completion_tokens', 0))
                    llm_calls += 1
                except:
                    pass
            
            if evt[3]:
                skills_used.append(evt[3])
        
        total_tokens = total_prompt + total_completion
        
        return {
            "session_id": session_id,
            "total_events": len(events),
            "user_queries": user_queries,
            "llm_calls": llm_calls,
            "tokens": {
                "prompt": total_prompt,
                "completion": total_completion,
                "total": total_tokens,
                "avg_per_call": total_tokens // llm_calls if llm_calls > 0 else 0,
            },
            "skills": {
                "unique": len(set(skills_used)),
                "total_calls": len(skills_used),
                "breakdown": dict((s, skills_used.count(s)) for s in set(skills_used)),
            },
        }
    
    def _get_event_breakdown(self, events) -> list[dict]:
        """Get detailed event breakdown."""
        breakdown = []
        for i, evt in enumerate(events, 1):
            entry = {
                "index": i,
                "type": evt[0],
                "model": evt[2],
                "skill": evt[3],
            }
            
            if evt[1]:
                try:
                    usage = json.loads(evt[1]) if isinstance(evt[1], str) else evt[1]
                    entry["tokens"] = usage.get('total', 0)
                except:
                    pass
            
            breakdown.append(entry)
        
        return breakdown
    
    def _generate_assessment(self, metrics: dict) -> dict:
        """Generate qualitative assessment."""
        tokens = metrics["tokens"]
        queries = metrics["user_queries"]
        llm_calls = metrics["llm_calls"]
        
        tokens_per_query = tokens["total"] // queries if queries > 0 else 0
        if tokens_per_query < 10000:
            token_efficiency = "excellent"
        elif tokens_per_query < 20000:
            token_efficiency = "good"
        elif tokens_per_query < 40000:
            token_efficiency = "moderate"
        else:
            token_efficiency = "needs_improvement"
        
        calls_per_query = llm_calls / queries if queries > 0 else 0
        if calls_per_query <= 2:
            call_efficiency = "excellent"
        elif calls_per_query <= 4:
            call_efficiency = "good"
        elif calls_per_query <= 6:
            call_efficiency = "moderate"
        else:
            call_efficiency = "needs_improvement"
        
        return {
            "token_efficiency": token_efficiency,
            "tokens_per_query": tokens_per_query,
            "call_efficiency": call_efficiency,
            "calls_per_query": round(calls_per_query, 1),
            "overall": "good" if token_efficiency in ("excellent", "good") and call_efficiency in ("excellent", "good") else "needs_improvement",
        }


# Register the skill
skill = EvaluateSessionSkill()
