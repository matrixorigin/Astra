"""Metrics for Tool Context Engine observability."""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from datetime import datetime
from typing import Dict


@dataclass
class ToolContextMetrics:
    """Metrics for tool context engine observability."""

    # Counters
    total_tool_outputs: int = 0
    summarized_outputs: int = 0
    direct_outputs: int = 0
    historical_reuse_hits: int = 0
    historical_reuse_misses: int = 0
    memory_expand_calls: int = 0
    staleness_rejections: int = 0

    # Size tracking
    total_input_bytes: int = 0
    total_output_bytes: int = 0  # After summarization

    # Per-tool breakdown
    per_tool_counts: Dict[str, int] = field(default_factory=dict)
    per_tool_summarized: Dict[str, int] = field(default_factory=dict)

    # Timestamps
    last_reset: datetime = field(default_factory=datetime.now)

    @property
    def summarization_rate(self) -> float:
        """Percentage of outputs that were summarized."""
        if self.total_tool_outputs == 0:
            return 0.0
        return self.summarized_outputs / self.total_tool_outputs

    @property
    def reuse_hit_rate(self) -> float:
        """Historical reuse hit rate."""
        total = self.historical_reuse_hits + self.historical_reuse_misses
        if total == 0:
            return 0.0
        return self.historical_reuse_hits / total

    @property
    def compression_ratio(self) -> float:
        """Compression ratio (input / output)."""
        if self.total_output_bytes == 0:
            return 1.0
        return self.total_input_bytes / self.total_output_bytes

    @property
    def expand_rate(self) -> float:
        """Rate of memory_expand calls per summarized output."""
        if self.summarized_outputs == 0:
            return 0.0
        return self.memory_expand_calls / self.summarized_outputs

    def to_dict(self) -> dict:
        """Export metrics as dict."""
        return {
            "total_tool_outputs": self.total_tool_outputs,
            "summarized_outputs": self.summarized_outputs,
            "direct_outputs": self.direct_outputs,
            "summarization_rate": round(self.summarization_rate, 3),
            "historical_reuse_hits": self.historical_reuse_hits,
            "historical_reuse_misses": self.historical_reuse_misses,
            "reuse_hit_rate": round(self.reuse_hit_rate, 3),
            "memory_expand_calls": self.memory_expand_calls,
            "expand_rate": round(self.expand_rate, 3),
            "staleness_rejections": self.staleness_rejections,
            "compression_ratio": round(self.compression_ratio, 2),
            "total_input_bytes": self.total_input_bytes,
            "total_output_bytes": self.total_output_bytes,
            "per_tool_counts": self.per_tool_counts,
            "last_reset": self.last_reset.isoformat(),
        }

    def reset(self) -> None:
        """Reset all metrics."""
        self.total_tool_outputs = 0
        self.summarized_outputs = 0
        self.direct_outputs = 0
        self.historical_reuse_hits = 0
        self.historical_reuse_misses = 0
        self.memory_expand_calls = 0
        self.staleness_rejections = 0
        self.total_input_bytes = 0
        self.total_output_bytes = 0
        self.per_tool_counts.clear()
        self.per_tool_summarized.clear()
        self.last_reset = datetime.now()


# Global singleton
_metrics = ToolContextMetrics()
_lock = threading.Lock()


def get_metrics() -> ToolContextMetrics:
    """Get global metrics instance."""
    return _metrics


def record_tool_output(
    tool_name: str, input_size: int, output_size: int, was_summarized: bool
) -> None:
    """Record a tool output processing event."""
    with _lock:
        _metrics.total_tool_outputs += 1
        _metrics.total_input_bytes += input_size
        _metrics.total_output_bytes += output_size
        _metrics.per_tool_counts[tool_name] = _metrics.per_tool_counts.get(tool_name, 0) + 1

        if was_summarized:
            _metrics.summarized_outputs += 1
            _metrics.per_tool_summarized[tool_name] = (
                _metrics.per_tool_summarized.get(tool_name, 0) + 1
            )
        else:
            _metrics.direct_outputs += 1


def record_reuse_attempt(hit: bool) -> None:
    """Record a historical reuse attempt."""
    with _lock:
        if hit:
            _metrics.historical_reuse_hits += 1
        else:
            _metrics.historical_reuse_misses += 1


def record_expand_call() -> None:
    """Record a memory_expand call."""
    with _lock:
        _metrics.memory_expand_calls += 1


def record_staleness_rejection() -> None:
    """Record a staleness rejection."""
    with _lock:
        _metrics.staleness_rejections += 1
