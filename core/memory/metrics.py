"""Memory metrics — simple in-process metrics collection."""

from __future__ import annotations

import time
import threading
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class MetricStats:
    """Statistics for a single metric."""

    count: int = 0
    total: float = 0.0
    min_val: float = float("inf")
    max_val: float = float("-inf")

    def record(self, value: float) -> None:
        self.count += 1
        self.total += value
        self.min_val = min(self.min_val, value)
        self.max_val = max(self.max_val, value)

    @property
    def avg(self) -> float:
        return self.total / self.count if self.count > 0 else 0.0

    def to_dict(self) -> dict:
        return {
            "count": self.count,
            "total": self.total,
            "avg": self.avg,
            "min": self.min_val if self.count > 0 else 0,
            "max": self.max_val if self.count > 0 else 0,
        }


class MemoryMetrics:
    """Thread-safe metrics collector for memory operations."""

    _instance: Optional["MemoryMetrics"] = None
    _lock = threading.Lock()

    def __new__(cls) -> "MemoryMetrics":
        if cls._instance is None:
            with cls._lock:
                if cls._instance is None:
                    cls._instance = super().__new__(cls)
                    cls._instance._init()
        return cls._instance

    def _init(self) -> None:
        self._metrics: dict[str, MetricStats] = defaultdict(MetricStats)
        self._counters: dict[str, int] = defaultdict(int)
        self._data_lock = threading.Lock()

    def record_latency(self, operation: str, latency_ms: float) -> None:
        """Record operation latency in milliseconds."""
        with self._data_lock:
            self._metrics[f"{operation}_latency_ms"].record(latency_ms)

    def increment(self, counter: str, value: int = 1) -> None:
        """Increment a counter."""
        with self._data_lock:
            self._counters[counter] += value

    def get_stats(self) -> dict:
        """Get all metrics as a dictionary."""
        with self._data_lock:
            return {
                "latencies": {k: v.to_dict() for k, v in self._metrics.items()},
                "counters": dict(self._counters),
            }

    def reset(self) -> None:
        """Reset all metrics."""
        with self._data_lock:
            self._metrics.clear()
            self._counters.clear()


# Singleton instance
metrics = MemoryMetrics()


class Timer:
    """Context manager for timing operations."""

    def __init__(self, operation: str):
        self.operation = operation
        self.start_time: float = 0

    def __enter__(self) -> "Timer":
        self.start_time = time.perf_counter()
        return self

    def __exit__(self, *args) -> None:
        elapsed_ms = (time.perf_counter() - self.start_time) * 1000
        metrics.record_latency(self.operation, elapsed_ms)
