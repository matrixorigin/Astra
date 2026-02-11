"""Planner for autonomous planning with PAOR loop.

Implements Plan-Act-Observe-Reflect loop for handling multi-step tasks.
"""

import json
import os
from typing import Any

from pydantic import BaseModel

from core.logging_config import get_logger
from core.events.models import StreamEvent, StreamEventType

logger = get_logger(__name__)


class PlanConstraints(BaseModel):
    """Constraints for plan execution."""

    max_steps: int = 10
    max_revisions: int = 3
    timeout_seconds: int = 300
    cost_budget_usd: float | None = None
    sandbox_required: bool = False


def get_plan_constraints() -> PlanConstraints:
    """Load plan constraints from environment variables."""
    return PlanConstraints(
        max_steps=int(os.getenv("MAX_PLAN_STEPS", "10")),
        max_revisions=int(os.getenv("MAX_PLAN_REVISIONS", "3")),
        timeout_seconds=int(os.getenv("PLAN_TIMEOUT_SECONDS", "300")),
        cost_budget_usd=float(os.getenv("PLAN_COST_BUDGET_USD", "0")) if os.getenv("PLAN_COST_BUDGET_USD") else None,
        sandbox_required=os.getenv("PLAN_SANDBOX_REQUIRED", "false").lower() == "true",
    )


class Planner:
    """Plans and manages PAOR loop execution."""

    def __init__(self, llm_client, constraints: PlanConstraints | None = None):
        self.llm = llm_client
        self.constraints = constraints or get_plan_constraints()

    async def create_plan(self, goal: str, context: str = "") -> dict:
        """Ask LLM to decompose goal into steps.
        
        Returns plan as dict for JSON serialization.
        """
        system_prompt = f"""You are a planning assistant. Your task is to break down complex goals into executable steps.

Goal: {goal}

Context: {context}

Instructions:
1. Create a clear, actionable plan with specific steps
2. Each step should be independent and executable
3. Specify dependencies between steps using 'depends_on'
4. Estimate which skill might be useful for each step
5. Keep plans under {self.constraints.max_steps} steps

Output format (JSON):
{{
    "plan_id": "unique_id",
    "goal": "original goal",
    "steps": [
        {{
            "step_id": "step_1",
            "description": "what to do",
            "skill_hint": "suggested_skill",
            "depends_on": []
        }}
    ]
}}"""

        messages = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": "Create a plan for the goal above."},
        ]

        try:
            response = self.llm.chat(
                messages=messages,
                user_id="system",
                session_id="planning",
            )

            # Parse JSON from response
            content = response.content.strip()
            if content.startswith("```json"):
                content = content[7:]
            if content.endswith("```"):
                content = content[:-3]
            content = content.strip()

            plan_data = json.loads(content)

            return plan_data

        except json.JSONDecodeError as e:
            logger.error(f"Failed to parse plan JSON: {e}")
            # Fallback: create a simple single-step plan
            return {
                "plan_id": "plan_001",
                "goal": goal,
                "steps": [
                    {
                        "step_id": "step_1",
                        "description": f"Execute: {goal}",
                    }
                ],
            }

    async def reflect(
        self, plan: dict, step_results: list[dict]
    ) -> tuple[str, dict | None]:
        """Evaluate progress and decide whether to revise plan.
        
        Returns: (assessment, revised_plan_or_None)
        """
        # Build reflection prompt
        plan_summary = f"Goal: {plan['goal']}\nSteps: {len(plan['steps'])}"
        results_summary = "\n".join(
            f"  - {r['step_id']}: {r.get('result', 'N/A')}"
            for r in step_results
        )

        system_prompt = f"""You are a planning assistant reviewing progress.

Current Plan:
{plan_summary}

Completed Steps Results:
{results_summary}

Instructions:
1. Assess whether the plan is progressing well
2. Identify any issues or blockers
3. Decide: continue, revise, or done
4. If revising, suggest specific changes

Output format (JSON):
{{
    "assessment": "brief assessment",
    "should_revise": true|false,
    "revised_steps": [optional revised step list]
}}"""

        messages = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": "Review the plan progress above."},
        ]

        try:
            response = self.llm.chat(
                messages=messages,
                user_id="system",
                session_id="planning",
            )

            content = response.content.strip()
            if content.startswith("```json"):
                content = content[7:]
            if content.endswith("```"):
                content = content[:-3]
            content = content.strip()

            review = json.loads(content)

            assessment = review.get("assessment", "Plan review complete")

            if review.get("should_revise", False) and review.get("revised_steps"):
                return assessment, {"revised_steps": review["revised_steps"]}

            return assessment, None

        except Exception as e:
            logger.error(f"Reflection failed: {e}")
            return "Review failed", None

    def get_next_steps(self, plan: dict) -> list[dict]:
        """Return steps whose dependencies are all completed."""
        completed_ids = {
            s["step_id"] for s in plan["steps"] if s.get("status") == "completed"
        }

        next_steps = []
        for step in plan["steps"]:
            if step.get("status") == "pending":
                deps = step.get("depends_on", [])
                if all(d in completed_ids for d in deps):
                    next_steps.append(step)

        return next_steps

    def check_constraints(self, plan: dict) -> tuple[bool, str | None]:
        """Check if plan violates any constraints.
        
        Returns: (is_valid, error_message)
        """
        if len(plan["steps"]) > self.constraints.max_steps:
            return False, f"Plan has {len(plan['steps'])} steps, max is {self.constraints.max_steps}"

        return True, None
