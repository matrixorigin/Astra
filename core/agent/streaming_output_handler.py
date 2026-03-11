"""Streaming output accumulator for Tool Context Engine.

Handles long-running commands (make test, docker build) that produce
streaming output of unknown total size.

Strategy:
1. Accumulate output in buffer
2. When buffer exceeds threshold, switch to "storage mode"
3. Store accumulated + future output in mo-trustmem
4. Return summary + reference
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

logger = logging.getLogger(__name__)

if TYPE_CHECKING:
    from collections.abc import AsyncIterator


@dataclass
class StreamAccumulatorState:
    """State of streaming accumulator."""

    buffer: str = ""
    total_bytes: int = 0
    switched_to_storage: bool = False
    memory_id: str | None = None
    line_count: int = 0


class StreamingOutputAccumulator:
    """Accumulate streaming output and switch to storage mode when needed."""

    def __init__(
        self,
        tool_name: str,
        session_id: str,
        user_id: str,
        memory_service: Any,
        threshold: int = 10 * 1024,
    ):
        self.tool_name = tool_name
        self.session_id = session_id
        self.user_id = user_id
        self.memory_service = memory_service
        self.threshold = threshold
        self.state = StreamAccumulatorState()

    def accumulate(self, chunk: str) -> str | None:
        """Accumulate a chunk of output.

        Returns None if still accumulating, or summary+reference if switched.
        Handles both pre-switch buffering and post-switch storage updates.
        """
        self.state.buffer += chunk
        self.state.total_bytes += len(chunk)
        self.state.line_count += chunk.count("\n")

        if not self.state.switched_to_storage and len(self.state.buffer) > self.threshold:
            self._switch_to_storage()
        elif self.state.switched_to_storage:  # noqa: SIM102
            # Post-switch: periodically flush buffer to storage (every 10KB)
            if len(self.state.buffer) % (10 * 1024) < len(chunk):
                self._update_storage()

        return None

    def _switch_to_storage(self) -> None:
        """Switch to storage mode - store buffer in mo-trustmem."""
        import uuid

        from core.memory.types import Memory, MemoryType

        self.state.switched_to_storage = True

        # Prefix content with tool provenance header so downstream consumers
        # (retrieval, audit) can identify which tool produced this output.
        header = f"[tool:{self.tool_name}] [streaming]\n"
        mem_obj = Memory(
            memory_id=uuid.uuid4().hex,
            user_id=self.user_id,
            memory_type=MemoryType.TOOL_RESULT,
            content=header + self.state.buffer,
            session_id=self.session_id,
            source_event_ids=[],
        )
        memory = self.memory_service.create_memory(mem_obj)
        self.state.memory_id = memory.memory_id

    def append_to_storage(self, chunk: str) -> None:
        """Append chunk to stored content (after switching to storage mode).

        .. deprecated:: Use accumulate() instead — it handles post-switch
           buffering and periodic flush internally.  Kept for backward compat.
        """
        # No-op: accumulate() now handles post-switch chunks.
        # Callers that still call both accumulate() + append_to_storage()
        # won't double-count bytes/lines.

    def _update_storage(self) -> None:
        """Update stored content with current buffer."""
        if not self.state.memory_id:
            return

        try:  # noqa: SIM105
            self.memory_service.update_memory_content(
                self.state.memory_id,
                self.state.buffer,
            )
        except Exception:
            # Non-critical: finalize() will do a final update.
            # Avoid logging per-chunk to prevent log spam during streaming.
            pass

    def finalize(self) -> str:
        """Finalize accumulation and return result.

        Returns full output if under threshold, or summary+reference if stored.
        """
        if not self.state.switched_to_storage:
            return self.state.buffer

        self._update_storage()

        # Re-embed with final content (initial embedding was from partial buffer)
        if self.state.memory_id:
            try:
                self.memory_service.update_memory_embedding(self.state.memory_id)
            except Exception:
                logger.warning("Re-embed failed for memory %s", self.state.memory_id, exc_info=True)

        summary = self._generate_summary()

        return f"{summary}\n\n[Full output ({self.state.total_bytes} bytes): memory:{self.state.memory_id}]"

    def _generate_summary(self) -> str:
        """Generate summary of accumulated output."""
        lines = self.state.buffer.split("\n")

        head = "\n".join(lines[:10])
        tail = "\n".join(lines[-5:]) if len(lines) > 15 else ""

        summary_parts = [
            f"Streaming output: {self.state.line_count} lines, {self.state.total_bytes} bytes",
            f"\nFirst 10 lines:\n{head}",
        ]

        if tail:
            summary_parts.append(f"\n...\nLast 5 lines:\n{tail}")

        error_lines = [line for line in lines if "error" in line.lower() or "fail" in line.lower()]
        if error_lines:
            summary_parts.append(f"\n⚠️ {len(error_lines)} error/fail lines detected")
            summary_parts.append("\n".join(error_lines[:3]))

        return "\n".join(summary_parts)


async def process_streaming_output(
    stream: AsyncIterator[str],
    tool_name: str,
    session_id: str,
    user_id: str,
    memory_service: Any,
    threshold: int = 10 * 1024,
) -> AsyncIterator[tuple[str, str | None]]:
    """Process streaming output, yielding chunks and final result.

    Yields (chunk, None) for each chunk while streaming,
    then (final_chunk, result) when stream ends.
    """
    accumulator = StreamingOutputAccumulator(
        tool_name, session_id, user_id, memory_service, threshold
    )

    last_chunk = ""
    async for chunk in stream:
        if last_chunk:
            yield (last_chunk, None)

        accumulator.accumulate(chunk)
        if accumulator.state.switched_to_storage:
            accumulator.append_to_storage(chunk)

        last_chunk = chunk

    result = accumulator.finalize()
    yield (last_chunk, result)
