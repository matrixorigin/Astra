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

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, AsyncIterator

if TYPE_CHECKING:
    from core.memory.store import MemoryStore


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
        memory_store: "MemoryStore",
        threshold: int = 10 * 1024,  # 10KB default
    ):
        self.tool_name = tool_name
        self.session_id = session_id
        self.user_id = user_id
        self.memory_store = memory_store
        self.threshold = threshold
        self.state = StreamAccumulatorState()
    
    def accumulate(self, chunk: str) -> str | None:
        """Accumulate a chunk of output.
        
        Args:
            chunk: New output chunk
        
        Returns:
            None if still accumulating, or summary+reference if switched to storage
        """
        self.state.buffer += chunk
        self.state.total_bytes += len(chunk)
        self.state.line_count += chunk.count('\n')
        
        # Check if should switch to storage mode
        if not self.state.switched_to_storage and len(self.state.buffer) > self.threshold:
            self._switch_to_storage()
        
        return None
    
    def _switch_to_storage(self) -> None:
        """Switch to storage mode - store buffer in mo-trustmem."""
        from core.memory.types import MemoryType
        
        self.state.switched_to_storage = True
        
        # Store current buffer
        memory = self.memory_store.create(
            user_id=self.user_id,
            content=self.state.buffer,
            memory_type=MemoryType.TOOL_RESULT,
            session_id=self.session_id,
            source=f"tool:{self.tool_name}:streaming",
            metadata={
                "tool": self.tool_name,
                "streaming": True,
                "partial": True,  # Will be updated on finalize
            },
        )
        self.state.memory_id = memory.memory_id
    
    def append_to_storage(self, chunk: str) -> None:
        """Append chunk to stored content (after switching to storage mode)."""
        if not self.state.switched_to_storage or not self.state.memory_id:
            return
        
        self.state.buffer += chunk
        self.state.total_bytes += len(chunk)
        self.state.line_count += chunk.count('\n')
        
        # Update stored content periodically (every 10KB)
        if len(self.state.buffer) % (10 * 1024) < len(chunk):
            self._update_storage()
    
    def _update_storage(self) -> None:
        """Update stored content with current buffer."""
        if not self.state.memory_id:
            return
        
        try:
            # Update content in place
            self.memory_store.update_content(
                self.state.memory_id,
                self.state.buffer,
            )
        except Exception:
            pass  # Non-critical, will finalize at end
    
    def finalize(self) -> str:
        """Finalize accumulation and return result.
        
        Returns:
            Full output if under threshold, or summary+reference if stored
        """
        if not self.state.switched_to_storage:
            # Under threshold - return full output
            return self.state.buffer
        
        # Update final content
        self._update_storage()
        
        # Update metadata to mark as complete
        try:
            self.memory_store.update_metadata(
                self.state.memory_id,
                {"partial": False, "final_size": self.state.total_bytes},
            )
        except Exception:
            pass
        
        # Generate summary
        summary = self._generate_summary()
        
        return f"{summary}\n\n[Full output ({self.state.total_bytes} bytes): memory:{self.state.memory_id}]"
    
    def _generate_summary(self) -> str:
        """Generate summary of accumulated output."""
        lines = self.state.buffer.split('\n')
        
        # Head + tail + stats
        head = '\n'.join(lines[:10])
        tail = '\n'.join(lines[-5:]) if len(lines) > 15 else ""
        
        summary_parts = [
            f"Streaming output: {self.state.line_count} lines, {self.state.total_bytes} bytes",
            f"\nFirst 10 lines:\n{head}",
        ]
        
        if tail:
            summary_parts.append(f"\n...\nLast 5 lines:\n{tail}")
        
        # Check for errors
        error_lines = [l for l in lines if 'error' in l.lower() or 'fail' in l.lower()]
        if error_lines:
            summary_parts.append(f"\n⚠️ {len(error_lines)} error/fail lines detected")
            summary_parts.append('\n'.join(error_lines[:3]))
        
        return '\n'.join(summary_parts)


async def process_streaming_output(
    stream: AsyncIterator[str],
    tool_name: str,
    session_id: str,
    user_id: str,
    memory_store: "MemoryStore",
    threshold: int = 10 * 1024,
) -> AsyncIterator[tuple[str, str | None]]:
    """Process streaming output, yielding chunks and final result.
    
    Args:
        stream: Async iterator of output chunks
        tool_name: Name of the tool
        session_id: Current session ID
        user_id: Current user ID
        memory_store: mo-trustmem MemoryStore
        threshold: Size threshold for switching to storage
    
    Yields:
        (chunk, None) for each chunk while streaming
        (final_chunk, result) when stream ends, where result is full output or summary+ref
    """
    accumulator = StreamingOutputAccumulator(
        tool_name, session_id, user_id, memory_store, threshold
    )
    
    last_chunk = ""
    async for chunk in stream:
        if last_chunk:
            yield (last_chunk, None)
        
        accumulator.accumulate(chunk)
        if accumulator.state.switched_to_storage:
            accumulator.append_to_storage(chunk)
        
        last_chunk = chunk
    
    # Finalize and yield last chunk with result
    result = accumulator.finalize()
    yield (last_chunk, result)
