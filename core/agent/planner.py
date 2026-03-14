"""Autonomous Planning with PAOR Loop.

Plan-Act-Observe-Reflect loop for handling multi-step tasks.
"""

import json
import os
from datetime import datetime
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field, ValidationError
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.utils.id_generator import generate_prefixed_id

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
    skill_params: dict[str, Any] | None = Field(
        default=None,
        description="Structured parameters for the skill (e.g. file_path, line_number)",
    )
    sub_plan: "Plan | None" = Field(
        default=None,
        description="Sub-plan for complex steps (hierarchical decomposition)",
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
    parent_plan_id: str | None = Field(
        default=None,
        description="Parent plan ID for sub-plans (hierarchical)",
    )
    depth: int = Field(
        default=0,
        description="Nesting depth (0=root, 1=sub-plan, etc.)",
    )
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
    max_depth: int = Field(default=3, description="Maximum nesting depth")
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
        max_depth=int(os.getenv("MAX_PLAN_DEPTH", "3")),
        max_revisions=int(os.getenv("MAX_PLAN_REVISIONS", "3")),
        timeout_seconds=int(os.getenv("PLAN_TIMEOUT_SECONDS", "300")),
        cost_budget_usd=float(os.getenv("PLAN_COST_BUDGET_USD", "0"))
        if os.getenv("PLAN_COST_BUDGET_USD")
        else None,
        sandbox_required=os.getenv("PLAN_SANDBOX_REQUIRED", "false").lower() == "true",
    )


class Planner:
    """Plans and manages PAOR loop execution."""

    def __init__(
        self, llm_client, constraints: PlanConstraints | None = None, event_logger=None, db=None
    ):
        self.llm = llm_client
        self.constraints = constraints or get_plan_constraints()
        self.event_logger = event_logger
        self.db = db  # For sandbox operations

    async def create_plan(
        self,
        goal: str,
        context: str = "",
        user_id: str | None = None,
        session_id: str | None = None,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> Plan:
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
                task_hint="planning",
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
            plan = Plan(**plan_data)

            # Log plan_created event if event_logger is available
            if self.event_logger and user_id and session_id:
                # Convert to dict with JSON-serializable types
                plan_dict = plan.model_dump()
                if "created_at" in plan_dict and isinstance(plan_dict["created_at"], datetime):
                    plan_dict["created_at"] = plan_dict["created_at"].isoformat()

                self.event_logger.create_plan_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="plan_created",
                    plan_data=plan_dict,
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                )

            return plan

        except json.JSONDecodeError as e:
            logger.error(f"Failed to parse plan JSON: {e}")
            # Fallback: create a simple single-step plan
            plan = Plan(
                plan_id=f"plan_{uuid7()}",
                goal=goal,
                steps=[
                    PlanStep(
                        step_id="step_1",
                        description=f"Execute: {goal}",
                    )
                ],
                constraints=self.constraints.model_dump(),
            )

            # Log fallback plan
            if self.event_logger and user_id and session_id:
                plan_dict = plan.model_dump()
                if "created_at" in plan_dict and isinstance(plan_dict["created_at"], datetime):
                    plan_dict["created_at"] = plan_dict["created_at"].isoformat()

                self.event_logger.create_plan_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="plan_created",
                    plan_data=plan_dict,
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={"fallback": True, "error": str(e)},
                )

            return plan
        except ValidationError as e:
            logger.error(f"Plan validation failed: {e}")
            # Fallback: create a simple single-step plan
            plan = Plan(
                plan_id=f"plan_{uuid7()}",
                goal=goal,
                steps=[
                    PlanStep(
                        step_id="step_1",
                        description=f"Execute: {goal}",
                    )
                ],
                constraints=self.constraints.model_dump(),
            )

            if self.event_logger and user_id and session_id:
                plan_dict = plan.model_dump()
                if "created_at" in plan_dict and isinstance(plan_dict["created_at"], datetime):
                    plan_dict["created_at"] = plan_dict["created_at"].isoformat()

                self.event_logger.create_plan_event(
                    user_id=user_id,
                    session_id=session_id,
                    event_type="plan_created",
                    plan_data=plan_dict,
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                    metadata={"fallback": True, "error": str(e)},
                )

            return plan

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
                task_hint="planning",
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

        if plan.depth > self.constraints.max_depth:
            return False, f"Plan depth {plan.depth} exceeds max {self.constraints.max_depth}"

        return True, None

    async def decompose_step(self, step: PlanStep, parent_plan: Plan) -> Plan | None:
        """Decompose a complex step into a sub-plan.

        Args:
            step: Step to decompose
            parent_plan: Parent plan containing this step

        Returns:
            Sub-plan or None if step is simple enough
        """
        # Check depth limit
        if parent_plan.depth >= self.constraints.max_depth:
            logger.warning(
                f"Max depth {self.constraints.max_depth} reached, skipping decomposition"
            )
            return None

        # Ask LLM if step needs decomposition
        system_prompt = f"""You are a planning assistant. Analyze if this step needs to be broken down into sub-steps.

Step: {step.description}
Parent Goal: {parent_plan.goal}
Current Depth: {parent_plan.depth}

If the step is simple and can be executed directly, respond with: {{"needs_decomposition": false}}
If the step is complex and needs sub-steps, respond with: {{"needs_decomposition": true, "sub_steps": [...]}}

Sub-steps format:
{{
    "needs_decomposition": true,
    "sub_steps": [
        {{"step_id": "sub_1", "description": "...", "skill_hint": "..."}},
        {{"step_id": "sub_2", "description": "...", "depends_on": ["sub_1"]}}
    ]
}}"""

        messages = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": f"Analyze step: {step.description}"},
        ]

        try:
            response = self.llm.chat(
                messages=messages, user_id="system", session_id="planning", task_hint="planning"
            )

            content = response.content.strip()
            if content.startswith("```json"):
                content = content[7:]
            if content.endswith("```"):
                content = content[:-3]
            content = content.strip()

            analysis = json.loads(content)

            if not analysis.get("needs_decomposition", False):
                return None

            # Create sub-plan
            sub_steps = [PlanStep(**s) for s in analysis.get("sub_steps", [])]

            sub_plan = Plan(
                plan_id=f"{parent_plan.plan_id}_sub_{step.step_id}",
                goal=step.description,
                steps=sub_steps,
                parent_plan_id=parent_plan.plan_id,
                depth=parent_plan.depth + 1,
                constraints=self.constraints.model_dump(),
            )

            logger.info(
                f"Decomposed step '{step.step_id}' into {len(sub_steps)} sub-steps "
                f"(depth {sub_plan.depth})"
            )

            return sub_plan

        except Exception as e:
            logger.error(f"Failed to decompose step: {e}")
            return None

    def log_step_start(
        self,
        step: PlanStep,
        plan_id: str,
        user_id: str,
        session_id: str,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> str | None:
        """Log plan step start event.

        Returns:
            Event ID if logged, None otherwise
        """
        if not self.event_logger:
            return None

        event = self.event_logger.create_plan_event(
            user_id=user_id,
            session_id=session_id,
            event_type="plan_step_start",
            plan_data={
                "plan_id": plan_id,
                "step_id": step.step_id,
                "description": step.description,
                "skill_hint": step.skill_hint,
            },
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
        )
        return event.event_id

    def log_step_done(
        self,
        step: PlanStep,
        plan_id: str,
        user_id: str,
        session_id: str,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> str | None:
        """Log plan step completion event.

        Returns:
            Event ID if logged, None otherwise
        """
        if not self.event_logger:
            return None

        event = self.event_logger.create_plan_event(
            user_id=user_id,
            session_id=session_id,
            event_type="plan_step_done",
            plan_data={
                "plan_id": plan_id,
                "step_id": step.step_id,
                "description": step.description,
                "status": step.status,
                "result": step.result,
                "reflection": step.reflection,
            },
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
        )
        return event.event_id

    def log_plan_completed(
        self,
        plan: Plan,
        user_id: str,
        session_id: str,
        summary: str,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> str | None:
        """Log plan completion event.

        Returns:
            Event ID if logged, None otherwise
        """
        if not self.event_logger:
            return None

        event = self.event_logger.create_plan_event(
            user_id=user_id,
            session_id=session_id,
            event_type="plan_completed",
            plan_data={
                "plan_id": plan.plan_id,
                "goal": plan.goal,
                "summary": summary,
                "total_steps": len(plan.steps),
                "completed_steps": sum(1 for s in plan.steps if s.status == PlanStatus.COMPLETED),
            },
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
        )
        return event.event_id

    def log_plan_failed(
        self,
        plan: Plan,
        user_id: str,
        session_id: str,
        reason: str,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> str | None:
        """Log plan failure event.

        Returns:
            Event ID if logged, None otherwise
        """
        if not self.event_logger:
            return None

        event = self.event_logger.create_plan_event(
            user_id=user_id,
            session_id=session_id,
            event_type="plan_failed",
            plan_data={
                "plan_id": plan.plan_id,
                "goal": plan.goal,
                "reason": reason,
                "completed_steps": sum(1 for s in plan.steps if s.status == PlanStatus.COMPLETED),
                "failed_step": next(
                    (s.step_id for s in plan.steps if s.status == PlanStatus.FAILED), None
                ),
            },
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
        )
        return event.event_id

    def log_plan_revised(
        self,
        revised_plan: Plan,
        user_id: str,
        session_id: str,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> str | None:
        """Log plan revision event.

        Returns:
            Event ID if logged, None otherwise
        """
        if not self.event_logger:
            return None

        # Convert plan to dict with JSON-serializable types
        plan_dict = revised_plan.model_dump()
        # Convert datetime to ISO string
        if "created_at" in plan_dict and isinstance(plan_dict["created_at"], datetime):
            plan_dict["created_at"] = plan_dict["created_at"].isoformat()

        event = self.event_logger.create_plan_event(
            user_id=user_id,
            session_id=session_id,
            event_type="plan_revised",
            plan_data=plan_dict,
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
        )
        return event.event_id


def restore_plan_from_events(db, goal_id: str) -> Plan | None:
    """Restore plan state from events by goal_id.

    Only restores plans that have NOT been completed or failed.

    Args:
        db: SQLAlchemy Session instance
        goal_id: Goal identifier (stored in metadata)

    Returns:
        Latest plan state or None if not found / already finished
    """
    from sqlalchemy import text

    # Query the latest plan for this goal
    result = db.execute(
        text("""
        SELECT event_id, event_type, content, created_at, metadata
        FROM agent_events
        WHERE event_type IN ('plan_created', 'plan_revised')
          AND JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.goal')) = :goal_id
        ORDER BY created_at DESC, event_id DESC
        LIMIT 1
        """),
        {"goal_id": goal_id},
    )
    rows = [dict(row._mapping) for row in result]

    if not rows:
        return None

    latest = rows[0]
    plan_data = json.loads(latest["content"])
    plan_id = plan_data["plan_id"]

    # Check if this plan was already completed or failed
    finished = db.execute(
        text("""
        SELECT 1 FROM agent_events
        WHERE event_type IN ('plan_completed', 'plan_failed')
          AND JSON_UNQUOTE(JSON_EXTRACT(content, '$.plan_id')) = :plan_id
        LIMIT 1
        """),
        {"plan_id": plan_id},
    )
    if finished.fetchone() is not None:
        return None

    # Restore step statuses from step events
    result = db.execute(
        text("""
        SELECT event_type, content
        FROM agent_events
        WHERE event_type IN ('plan_step_start', 'plan_step_done')
          AND JSON_UNQUOTE(JSON_EXTRACT(content, '$.plan_id')) = :plan_id
        ORDER BY created_at
        """),
        {"plan_id": plan_data["plan_id"]},
    )
    step_events = [dict(row._mapping) for row in result]

    # Update step statuses
    step_status_map = {}
    for event in step_events:
        event_data = json.loads(event["content"])
        step_id = event_data["step_id"]

        if event["event_type"] == "plan_step_start":
            step_status_map[step_id] = {
                "status": "in_progress",
            }
        elif event["event_type"] == "plan_step_done":
            step_status_map[step_id] = {
                "status": event_data.get("status", "completed"),
                "result": event_data.get("result"),
                "reflection": event_data.get("reflection"),
            }

    # Apply statuses to plan
    for step in plan_data.get("steps", []):
        step_id = step["step_id"]
        if step_id in step_status_map:
            step.update(step_status_map[step_id])

    return Plan(**plan_data)


def execute_plan_in_sandbox(
    plan: Plan,
    db,
    executor_fn,
    sandbox_name: str | None = None,
) -> dict:
    """Execute plan in sandbox for dry-run validation.

    Args:
        plan: Plan to execute
        db: SQLAlchemy Session
        executor_fn: Function to execute each step (step -> result)
        sandbox_name: Optional sandbox name (auto-generated if None)

    Returns:
        dict with execution results and sandbox info

    Example:
        >>> def my_executor(step):
        ...     # Execute step logic
        ...     return {"success": True, "output": "..."}
        >>>
        >>> result = execute_plan_in_sandbox(
        ...     plan=plan,
        ...     db=db,
        ...     executor_fn=my_executor,
        ... )
        >>> print(result["sandbox_name"])  # "plan_dry_run_abc123"
        >>> print(result["success"])  # True/False
    """
    from core.sandbox.sandbox import Sandbox

    # Generate sandbox name if not provided (use timestamp + random for uniqueness)
    if sandbox_name is None:
        sandbox_name = generate_prefixed_id("plan_dry_run")

    sandbox = Sandbox(db_factory=lambda: db)
    results = {
        "sandbox_name": sandbox_name,
        "plan_id": plan.plan_id,
        "success": True,
        "steps": [],
        "error": None,
    }

    try:
        # Create sandbox
        sandbox.create(
            name=sandbox_name,
            description=f"Dry-run for plan {plan.plan_id}: {plan.goal}",
            created_by="planner",
            tags=["dry-run", "plan", plan.plan_id],
        )

        # Execute each step in sandbox context
        for step in plan.steps:
            try:
                # Execute step
                step_result = executor_fn(step)

                results["steps"].append(
                    {
                        "step_id": step.step_id,
                        "description": step.description,
                        "success": True,
                        "result": step_result,
                    }
                )

            except Exception as e:
                results["success"] = False
                results["steps"].append(
                    {
                        "step_id": step.step_id,
                        "description": step.description,
                        "success": False,
                        "error": str(e),
                    }
                )
                # Stop on first failure
                break

        # Cleanup: delete sandbox after dry-run
        sandbox.delete(sandbox_name)

    except Exception as e:
        results["success"] = False
        results["error"] = str(e)

        # Cleanup on error
        try:
            sandbox.delete(sandbox_name)
        except Exception:
            pass

    return results
