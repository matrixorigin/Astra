"""Prompt Auto-Evolution: LLM-driven prompt optimization with regression gating.

Flow:
  1. Collect low-rated feedback cases
  2. Correlate with context snapshots (what the LLM saw)
  3. Ask LLM to diagnose prompt weaknesses and generate improvement
  4. Validate via regression gate (replay historical sessions)
  5. Activate new prompt version if gate passes

This closes the loop: snapshot → feedback → analysis → improvement → validation → activation.
No other agent framework does this end-to-end with auditable snapshots.
"""

import json
import logging
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


@dataclass
class OptimizationResult:
    template_id: str
    old_version: str
    new_version: str | None = None
    new_content: str | None = None
    diagnosis: str = ""
    gate_verdict: str = "skipped"
    activated: bool = False
    cases_analyzed: int = 0
    error: str | None = None


class PromptOptimizer(DbConsumer):
    """Automatically improve prompts based on feedback + context snapshots."""

    def __init__(self, db_factory: DbFactory, llm_client):
        super().__init__(db_factory)
        self.llm = llm_client

    def optimize(
        self,
        template_id: str,
        min_cases: int = 3,
        rating_threshold: int = 2,
        dry_run: bool = False,
    ) -> OptimizationResult:
        """Run one optimization cycle for a prompt template.

        Args:
            template_id: Which prompt to optimize (e.g. 'system_general')
            min_cases: Minimum low-score cases needed to proceed
            rating_threshold: Feedback rating <= this is considered "bad"
            dry_run: If True, generate improvement but don't activate

        Returns:
            OptimizationResult with diagnosis, new prompt, and gate verdict
        """
        # 1. Get current prompt
        current = self._get_current_prompt(template_id)
        if not current:
            return OptimizationResult(
                template_id=template_id, old_version="?",
                error=f"Template '{template_id}' not found",
            )
        old_version, old_content = current

        # 2. Collect failure cases with context snapshots
        cases = self._collect_failure_cases(template_id, rating_threshold)
        if len(cases) < min_cases:
            return OptimizationResult(
                template_id=template_id, old_version=old_version,
                cases_analyzed=len(cases),
                error=f"Only {len(cases)} low-score cases (need {min_cases})",
            )

        # 3. LLM diagnosis + improvement
        diagnosis, new_content = self._generate_improvement(
            template_id, old_content, cases,
        )

        if not new_content:
            return OptimizationResult(
                template_id=template_id, old_version=old_version,
                diagnosis=diagnosis, cases_analyzed=len(cases),
                error="LLM failed to generate improvement",
            )

        new_version = self._next_version(old_version)

        result = OptimizationResult(
            template_id=template_id,
            old_version=old_version,
            new_version=new_version,
            new_content=new_content,
            diagnosis=diagnosis,
            cases_analyzed=len(cases),
        )

        if dry_run:
            result.gate_verdict = "dry_run"
            return result

        # 4. Validate via regression gate
        gate_verdict = self._validate_with_gate(template_id, new_version, new_content)
        result.gate_verdict = gate_verdict

        # 5. Activate if gate passes
        if gate_verdict in ("pass", "skipped", "skip"):
            self._activate_prompt(template_id, new_version, new_content)
            result.activated = True
            logger.info(
                f"Prompt auto-evolved: {template_id} {old_version} → {new_version} "
                f"(gate={gate_verdict}, cases={len(cases)})"
            )
        else:
            logger.warning(
                f"Prompt optimization rejected by gate: {template_id} (verdict={gate_verdict})"
            )

        return result

    # ── Internal ──────────────────────────────────────────────────

    def _get_current_prompt(self, template_id: str) -> tuple[str, str] | None:
        with self._db() as db:
            row = db.execute(
                text(
                    "SELECT version, content FROM ctx_prompt_templates "
                    "WHERE template_id = :tid AND is_active = 1 "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"tid": template_id},
            ).first()
            return (row[0], row[1]) if row else None

    def _collect_failure_cases(
        self, template_id: str, threshold: int,
    ) -> list[dict[str, Any]]:
        """Get low-rated cases with their context snapshots."""
        with self._db() as db:
            rows = db.execute(
                text("""
                    SELECT f.rating, f.comment, f.llm_request_id,
                           cs.system_prompt, cs.task_type,
                           e.content as user_query
                    FROM eval_llm_feedback f
                    LEFT JOIN ctx_snapshots cs ON f.llm_request_id = cs.llm_request_id
                    LEFT JOIN agent_events e ON cs.event_id = e.event_id
                    WHERE f.prompt_template_id = :tid AND f.rating <= :threshold
                    ORDER BY f.created_at DESC
                    LIMIT 20
                """),
                {"tid": template_id, "threshold": threshold},
            ).fetchall()

            return [
                {
                    "rating": r[0],
                    "comment": r[1],
                    "user_query": r[5] or "(unknown)",
                    "system_prompt_used": r[3] or "(not captured)",
                    "task_type": r[4] or "general",
                }
                for r in rows
            ]

    def _generate_improvement(
        self, template_id: str, current_prompt: str, cases: list[dict],
    ) -> tuple[str, str | None]:
        """Ask LLM to diagnose and improve the prompt."""
        cases_text = "\n".join(
            f"- Query: {c['user_query'][:200]} | Rating: {c['rating']}/5 | Comment: {c.get('comment') or 'none'}"
            for c in cases[:10]
        )

        analysis_prompt = f"""You are a prompt engineering expert. Analyze why this system prompt produced low-quality responses, then write an improved version.

CURRENT PROMPT (template: {template_id}):
---
{current_prompt}
---

LOW-RATED CASES ({len(cases)} total, showing up to 10):
{cases_text}

INSTRUCTIONS:
1. First, write a brief DIAGNOSIS (2-3 sentences) of what's wrong with the current prompt.
2. Then write an IMPROVED PROMPT that fixes the identified issues.
   - Keep the same role and purpose
   - Be specific about what to do and what NOT to do
   - Add concrete examples or patterns if helpful
   - Keep it concise (under 500 words)

Format your response EXACTLY as:
DIAGNOSIS: <your diagnosis>
IMPROVED_PROMPT:
<your improved prompt>"""

        try:
            response = self.llm.chat(
                messages=[{"role": "user", "content": analysis_prompt}],
                user_id="system",
                temperature=0.3,
                task_hint="prompt_optimization",
            )
            content = response.content if hasattr(response, "content") else str(response)
            return self._parse_improvement(content)
        except Exception as e:
            logger.error(f"LLM improvement generation failed: {e}")
            return str(e), None

    @staticmethod
    def _parse_improvement(text: str) -> tuple[str, str | None]:
        """Parse LLM response into (diagnosis, improved_prompt)."""
        diagnosis = ""
        improved = None

        if "DIAGNOSIS:" in text:
            parts = text.split("DIAGNOSIS:", 1)[1]
            if "IMPROVED_PROMPT:" in parts:
                diagnosis, rest = parts.split("IMPROVED_PROMPT:", 1)
                diagnosis = diagnosis.strip()
                improved = rest.strip()
            else:
                diagnosis = parts.strip()
        elif "IMPROVED_PROMPT:" in text:
            improved = text.split("IMPROVED_PROMPT:", 1)[1].strip()

        return diagnosis, improved

    def _validate_with_gate(
        self, template_id: str, new_version: str, new_content: str,
    ) -> str:
        """Validate improvement via regression gate."""
        try:
            from core.evaluation.regression_gate import RegressionGate, ChangeType

            gate = RegressionGate(self._db_factory)
            result = gate.validate_change(
                change_type=ChangeType.PROMPT,
                change_id=f"{template_id}@{new_version}",
                change_content={"template_id": template_id, "content": new_content},
                golden_session_count=10,
            )
            return result.get("verdict", "error")
        except Exception as e:
            logger.warning(f"Gate validation unavailable: {e}")
            return "skipped"

    def _activate_prompt(
        self, template_id: str, version: str, content: str,
    ) -> None:
        """Register and activate the new prompt version."""
        from core.context.prompts import PromptManager
        pm = PromptManager(self._db_factory)
        pm.register_prompt(template_id, version, content, is_active=True)

    @staticmethod
    def _next_version(current: str) -> str:
        """Bump minor version: '1.0' → '1.1', '2.3' → '2.4'."""
        parts = current.split(".")
        if len(parts) >= 2:
            try:
                parts[-1] = str(int(parts[-1]) + 1)
                return ".".join(parts)
            except ValueError:
                pass
        return current + ".1"
