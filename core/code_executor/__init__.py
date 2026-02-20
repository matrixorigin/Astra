"""CodeExecutor — orchestration service composing Runtime + DataContext + SecurityGuard."""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field

from sqlalchemy.orm import Session

from core.code_executor.data_context import (
    DataAccessLevel,
    DataContext,
    DataContextScope,
    TableDiff,
)
from core.code_executor.security import SecurityGuard, SecurityVerdict
from core.runtime import ExecutionResult, ResourceProfile, Runtime
from core.sandbox.sandbox import Sandbox


@dataclass
class CodeExecutionRequest:
    code: str
    language: str = "python"
    resources: ResourceProfile = field(default_factory=ResourceProfile)
    session_id: str | None = None
    data_access: DataAccessLevel = DataAccessLevel.NONE
    data_scope: DataContextScope = DataContextScope.EXECUTION
    allowed_imports: list[str] | None = None


@dataclass
class CodeExecutionResult:
    execution: ExecutionResult
    security: SecurityVerdict
    data_diff: list[TableDiff] | None = None


class CodeExecutor:
    """Orchestrates security check → data context → runtime execution → cleanup.

    Callers (skills, ChatLoop) interact only with this service.
    """

    def __init__(
        self,
        runtime: Runtime,
        db: Session,
        sandbox: Sandbox,
        security: SecurityGuard | None = None,
    ):
        self.runtime = runtime
        self.db = db
        self.sandbox = sandbox
        self.security = security or SecurityGuard()
        # Session-scoped DataContexts keyed by session_id
        self._session_contexts: dict[str, DataContext] = {}

    def execute(self, request: CodeExecutionRequest) -> CodeExecutionResult:
        """Execute code with security check, optional data context, and cleanup."""

        # 1. GUARD
        verdict = self.security.analyze(
            request.code, request.language, request.allowed_imports,
        )
        if not verdict.safe:
            return CodeExecutionResult(
                execution=ExecutionResult(
                    stdout="", stderr="Security check failed: " + "; ".join(
                        f"[L{i.line}] {i.description}" for i in verdict.issues
                    ),
                    exit_code=1, execution_time_ms=0,
                ),
                security=verdict,
            )

        # 2. DATA
        context: DataContext | None = None
        env: dict[str, str] = {}

        if request.data_access != DataAccessLevel.NONE:
            context = self._get_or_create_context(
                request.session_id, request.data_access, request.data_scope,
            )
            context.ensure_created()
            if request.data_access == DataAccessLevel.WRITE:
                context.checkpoint("pre_exec")
            env["MO_DSN"] = context.dsn
            env["MO_DATABASE"] = context.sandbox_name

        # 3. EXECUTE
        try:
            result = self.runtime.execute(
                request.code, request.language, request.resources, env or None,
            )
        except Exception as e:
            # Runtime failure — restore if WRITE
            if context and request.data_access == DataAccessLevel.WRITE:
                try:
                    context.restore("pre_exec")
                except Exception:
                    pass
            if context and request.data_scope == DataContextScope.EXECUTION:
                context.destroy()
            return CodeExecutionResult(
                execution=ExecutionResult(
                    stdout="", stderr=f"Runtime error: {e}",
                    exit_code=1, execution_time_ms=0,
                ),
                security=verdict,
            )

        # 4. POST-EXECUTE
        data_diff: list[TableDiff] | None = None
        if context and request.data_access == DataAccessLevel.WRITE:
            if result.exit_code != 0:
                try:
                    context.restore("pre_exec")
                except Exception:
                    pass
            else:
                data_diff = context.diff()

        # 5. CLEANUP (execution-scoped only) — always runs
        if context and request.data_scope == DataContextScope.EXECUTION:
            context.destroy()

        return CodeExecutionResult(
            execution=result,
            security=verdict,
            data_diff=data_diff,
        )

    def cleanup_session(self, session_id: str) -> None:
        """Destroy session-scoped DataContext. Call when session closes."""
        ctx = self._session_contexts.pop(session_id, None)
        if ctx:
            ctx.destroy()

    def _get_or_create_context(
        self,
        session_id: str | None,
        access: DataAccessLevel,
        scope: DataContextScope,
    ) -> DataContext:
        # Session-scoped: reuse existing
        if scope == DataContextScope.SESSION and session_id:
            if session_id in self._session_contexts:
                return self._session_contexts[session_id]

            ctx = DataContext(
                db=self.db,
                sandbox=self.sandbox,
                sandbox_name=f"code_exec_{session_id[:8]}",
                access=access,
                scope=scope,
            )
            self._session_contexts[session_id] = ctx
            return ctx

        # Execution-scoped: always create new
        return DataContext(
            db=self.db,
            sandbox=self.sandbox,
            sandbox_name=f"code_exec_{uuid.uuid4().hex[:8]}",
            access=access,
            scope=scope,
        )
