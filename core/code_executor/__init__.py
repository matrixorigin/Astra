"""CodeExecutor — orchestration service composing Runtime + DataContext + SecurityGuard."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime


from core.utils.id_generator import generate_hash_id
from core.code_executor.data_context import (
    DataAccessLevel,
    DataContext,
    TableDiff,
)
from core.code_executor.security import SecurityGuard, SecurityVerdict
from core.runtime import ExecutionResult, ResourceProfile, Runtime
from core.sandbox.branch import Branch
from core.db_consumer import DbConsumer, DbFactory


@dataclass
class CodeExecutionRequest:
    code: str
    language: str = "python"
    resources: ResourceProfile = field(default_factory=ResourceProfile)
    session_id: str | None = None
    data_access: DataAccessLevel = DataAccessLevel.NONE
    source_db: str | None = None  # Required for READ/WRITE
    tables: list[str] | None = None  # Required for WRITE — declares accessed tables
    allowed_imports: list[str] | None = None


@dataclass
class TimeTravelInfo:
    """Records what's needed to audit/reproduce an execution."""

    started_at: datetime  # Execution start UTC (PITR within GC window)
    source_db: str  # Source database name
    sandbox_db: str  # Sandbox database name


@dataclass
class CodeExecutionResult:
    execution: ExecutionResult
    security: SecurityVerdict
    data_diff: list[TableDiff] | None = None
    time_travel: TimeTravelInfo | None = None  # Only for WRITE mode


class CodeExecutor(DbConsumer):
    """Orchestrates security check → data context → runtime execution.

    DataContext is session-scoped only. Uses data branch for zero-copy table branching.
    """

    def __init__(
        self,
        runtime: Runtime,
        db_factory: DbFactory,
        branch: Branch | None = None,
        security: SecurityGuard | None = None,
    ):
        self.runtime = runtime
        super().__init__(db_factory)
        self.branch = branch
        self.security = security or SecurityGuard()
        self._session_contexts: dict[str, DataContext] = {}

    def execute(self, request: CodeExecutionRequest) -> CodeExecutionResult:
        # 1. GUARD
        verdict = self.security.analyze(
            request.code,
            request.language,
            request.allowed_imports,
        )
        if not verdict.safe:
            return CodeExecutionResult(
                execution=ExecutionResult(
                    stdout="",
                    stderr="Security check failed: "
                    + "; ".join(f"[L{i.line}] {i.description}" for i in verdict.issues),
                    exit_code=1,
                    execution_time_ms=0,
                ),
                security=verdict,
            )

        # 2. DATA
        context: DataContext | None = None
        env: dict[str, str] = {}

        if request.data_access == DataAccessLevel.READ:
            if request.source_db:
                env["MO_DATABASE"] = request.source_db

        elif request.data_access == DataAccessLevel.WRITE:
            if not request.session_id:
                raise ValueError("WRITE mode requires session_id")
            if not request.source_db:
                raise ValueError("WRITE mode requires source_db")
            if not request.tables:
                raise ValueError("WRITE mode requires tables")

            context = self._get_or_create_context(
                request.session_id,
                request.source_db,
            )
            context.ensure_created()
            context.ensure_tables(request.tables)
            env["MO_DSN"] = context.dsn
            env["MO_DATABASE"] = context.sandbox_name

        # 3. EXECUTE — inject runtime capabilities as env vars
        cap = self.runtime.capabilities
        env["MO_RUNTIME_ISOLATION"] = cap.isolation.value
        env["MO_RUNTIME_NETWORK"] = "1" if cap.network_isolatable else "0"
        env["MO_RUNTIME_FS_ISOLATED"] = "1" if cap.filesystem_isolated else "0"
        env["MO_RUNTIME_RESOURCE_LIMITS"] = "1" if cap.resource_limits else "0"

        try:
            result = self.runtime.execute(
                request.code,
                request.language,
                request.resources,
                env or None,
            )
        except Exception as e:
            return CodeExecutionResult(
                execution=ExecutionResult(
                    stdout="",
                    stderr=f"Runtime error: {e}",
                    exit_code=1,
                    execution_time_ms=0,
                ),
                security=verdict,
            )

        # 4. POST-EXECUTE
        data_diff: list[TableDiff] | None = None
        time_travel: TimeTravelInfo | None = None

        if context and request.data_access == DataAccessLevel.WRITE:
            if result.exit_code == 0:
                data_diff = context.diff(request.tables)
            if result.started_at:
                time_travel = TimeTravelInfo(
                    started_at=result.started_at,
                    source_db=request.source_db,
                    sandbox_db=context.sandbox_name,
                )

        return CodeExecutionResult(
            execution=result,
            security=verdict,
            data_diff=data_diff,
            time_travel=time_travel,
        )

    def cleanup_session(self, session_id: str) -> None:
        """Destroy session-scoped DataContext. Call when session closes."""
        ctx = self._session_contexts.pop(session_id, None)
        if ctx:
            ctx.destroy()

    def _get_or_create_context(
        self,
        session_id: str,
        source_db: str,
    ) -> DataContext:
        if session_id in self._session_contexts:
            return self._session_contexts[session_id]

        ctx = DataContext(
            db_factory=self._db_factory,
            branch=self.branch,
            sandbox_name=f"code_exec_{generate_hash_id(session_id, 8)}",
            source_db=source_db,
            access=DataAccessLevel.WRITE,
            session_id=session_id,
        )
        self._session_contexts[session_id] = ctx
        return ctx
