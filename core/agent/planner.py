"""Autonomous Planning with PAOR Loop.

Plan-Act-Observe-Reflect loop for handling multi-step tasks.
"""

import json
import os
from datetime import datetime
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field, ValidationError

from core.logging_config import get_logger

logger = get_logger(__name__)


class PlanStatus(str, Enum):
    """Status of a plan or plan step."""

    PENDING = "pending"
    IN_PROGRESS = "in_progress"
    COMPLETED = "completed"
    FAILED = "failed"
    REVISED = "revised"


class PlanStep(BaseModel):
    """A single step in a plan."""

    step_id: str = Field(description="Unique step identifier")
    description: str = Field(description="Human-readable description")
    skill_hint: str | None = Field(
        default=None,
        description="Suggested skill to use (optional)",
    )
    depends_on: list[str] = Field(
        default_factory=list,
        description="Step IDs this step depends on",
    )
    status: PlanStatus = PlanStatus.PENDING
    result: str | None = Field(
        default=None,
        description="Result of executing this step",
    )
    reflection: str | None = Field(
        default=None,
        description="Agent's assessment after execution",
    )


class Plan(BaseModel):
    """A plan for achieving a goal."""

    plan_id: str = Field(description="Unique plan identifier")
    goal: str = Field(description="The goal this plan aims to achieve")
    steps: list[PlanStep] = Field(description="Ordered steps to execute")
    revision_of: str | None = Field(
        default=None,
        description="If this revises a previous plan, reference to it",
    )
    created_at: datetime = Field(default_factory=datetime.now)
    constraints: dict[str, Any] = Field(
        default_factory=dict,
        description="Plan constraints (max_steps, max_revisions, etc.)",
    )


class PlanConstraints(BaseModel):
    """Constraints for plan execution."""

    max_steps: int = Field(default=10, description="Maximum steps in a plan")
    max_revisions: int = Field(default=3, description="Maximum plan revisions")
    timeout_seconds: int = Field(default=300, description="Execution timeout")
    cost_budget_usd: float | None = Field(
        default=None,
        description="Cost budget in USD",
    )
    sandbox_required: bool = Field(
        default=False,
        description="Force execution in sandbox branch",
    )


def get_plan_constraints() -> PlanConstraints:
    """Load plan constraints from environment variables."""
    return PlanConstraints(
        max_steps=int(os.getenv("MAX_PLAN_STEPS", "10")),
        max_revisions=int(os.getenv("MAX_PLAN_REVISIONS", "3")),
        timeout_seconds=int(os.getenv("PLAN_TIMEOUT_SECONDS", "300")),
        cost_budget_usd=float(os.getenv("PLAN_COST_BUDGET_USD", "0"))
        if os.getenv("PLAN_COST_BUDGET_USD")
        else None,
        sandbox_required=os.getenv("PLAN_SANDBOX_REQUIRED", "false").lower() == "true",
    )


class Planner:
    """Plans and manages PAOR loop execution."""

    def __init__(self, llm_client, constraints: PlanConstraints | None = None):
        self.llm = llm_client
        self.constraints = constraints or get_plan_constraints()

    async def create_plan(self, goal: str, context: str = "") -> Plan:
        """Ask LLM to decompose goal into steps.

        Returns Plan Pydantic model with validation.
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

            # Validate and create Plan model
            return Plan(**plan_data)

        except json.JSONDecodeError as e:
            logger.error(f"Failed to parse plan JSON: {e}")
            # Fallback: create a simple single-step plan
            return Plan(
                plan_id="plan_001",
                goal=goal,
                steps=[
                    PlanStep(
                        step_id="step_1",
                        description=f"Execute: {goal}",
                    )
                ],
                constraints=self.constraints.model_dump(),
            )
        except ValidationError as e:
            logger.error(f"Plan validation failed: {e}")
            # Fallback: create a simple single-step plan
            return Plan(
                plan_id="plan_001",
                goal=goal,
                steps=[
                    PlanStep(
                        step_id="step_1",
                        description=f"Execute: {goal}",
                    )
                ],
                constraints=self.constraints.model_dump(),
            )

    async def reflect(self, plan: Plan, step_results: list[dict]) -> tuple[str, Plan | None]:
        """Evaluate progress and decide whether to revise plan.

        Returns: (assessment, revised_plan_or_None)
        """
        # Build reflection prompt
        plan_summary = f"Goal: {plan.goal}\nSteps: {len(plan.steps)}"
        results_summary = "\n".join(
            f"  - {r['step_id']}: {r.get('result', 'N/A')}" for r in step_results
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
                # Validate and create revised Plan model
                try:
                    revised_steps = [PlanStep(**s) for s in review["revised_steps"]]
                    revised_plan = Plan(
                        plan_id=f"{plan.plan_id}_rev_{len([s for s in plan.steps if s.status == PlanStatus.REVISED]) + 1}",
                        goal=plan.goal,
                        steps=revised_steps,
                        revision_of=plan.plan_id,
                        constraints=self.constraints.model_dump(),
                    )
                    return assessment, revised_plan
                except ValidationError as e:
                    logger.error(f"Revised plan validation failed: {e}")
                    return assessment, None

            return assessment, None

        except Exception as e:
            logger.error(f"Reflection failed: {e}")
            return "Review failed", None

    def get_next_steps(self, plan: Plan) -> list[PlanStep]:
        """Return steps whose dependencies are all completed."""
        completed_ids = {s.step_id for s in plan.steps if s.status == PlanStatus.COMPLETED}

        next_steps = []
        for step in plan.steps:
            if step.status == PlanStatus.PENDING:
                # Check if all dependencies are completed
                deps = step.depends_on or []
                if all(d in completed_ids for d in deps):
                    next_steps.append(step)

        return next_steps

    def check_constraints(self, plan: Plan) -> tuple[bool, str | None]:
        """Check if plan violates any constraints.

        Returns: (is_valid, error_message)
        """
        if len(plan.steps) > self.constraints.max_steps:
            return False, f"Plan has {len(plan.steps)} steps, max is {self.constraints.max_steps}"

        return True, None
