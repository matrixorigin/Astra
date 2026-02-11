"""Rate limiter with token buckets and circuit breaker."""

import time
import logging
import threading

logger = logging.getLogger(__name__)


class TokenBucket:
    """Token bucket that refills per minute."""

    def __init__(self, capacity: int):
        self.capacity = capacity
        self.tokens = float(capacity)
        self.last_refill = time.monotonic()
        self._lock = threading.Lock()

    def acquire(self, amount: int = 1) -> bool:
        with self._lock:
            self._refill()
            if self.tokens >= amount:
                self.tokens -= amount
                return True
            return False

    def _refill(self):
        now = time.monotonic()
        elapsed = now - self.last_refill
        self.tokens = min(self.capacity, self.tokens + elapsed * (self.capacity / 60.0))
        self.last_refill = now


class CircuitBreaker:
    """Per-provider circuit breaker (#6).

    States: CLOSED (normal) → OPEN (failing) → HALF_OPEN (probing).
    """

    def __init__(self, failure_threshold: int = 5, recovery_timeout: float = 60.0):
        self.failure_threshold = failure_threshold
        self.recovery_timeout = recovery_timeout
        self._failures = 0
        self._last_failure_time = 0.0
        self._state = "closed"  # closed | open | half_open
        self._lock = threading.Lock()

    @property
    def state(self) -> str:
        with self._lock:
            if self._state == "open":
                if time.monotonic() - self._last_failure_time > self.recovery_timeout:
                    self._state = "half_open"
            return self._state

    def allow_request(self) -> bool:
        s = self.state
        if s == "closed":
            return True
        if s == "half_open":
            return True  # allow one probe request
        return False  # open — reject

    def record_success(self):
        with self._lock:
            self._failures = 0
            self._state = "closed"

    def record_failure(self):
        with self._lock:
            self._failures += 1
            self._last_failure_time = time.monotonic()
            if self._failures >= self.failure_threshold:
                self._state = "open"
                logger.warning(f"Circuit breaker OPEN after {self._failures} failures")


class RateLimiter:
    """Dual-bucket rate limiter (RPM + TPM) with circuit breaker per provider."""

    def __init__(self):
        self._rpm_buckets: dict[str, TokenBucket] = {}
        self._tpm_buckets: dict[str, TokenBucket] = {}
        self._breakers: dict[str, CircuitBreaker] = {}

    def configure(self, model: str, rpm: int, tpm: int):
        self._rpm_buckets[model] = TokenBucket(rpm)
        self._tpm_buckets[model] = TokenBucket(tpm)

    def configure_breaker(
        self, provider: str, failure_threshold: int = 5, recovery_timeout: float = 60.0
    ):
        self._breakers[provider] = CircuitBreaker(failure_threshold, recovery_timeout)

    def get_breaker(self, provider: str) -> CircuitBreaker:
        if provider not in self._breakers:
            self._breakers[provider] = CircuitBreaker()
        return self._breakers[provider]

    def acquire(self, model: str, estimated_tokens: int = 1) -> bool:
        rpm = self._rpm_buckets.get(model)
        tpm = self._tpm_buckets.get(model)
        if not rpm or not tpm:
            return True
        if not rpm.acquire(1):
            logger.warning(f"Rate limited (RPM): {model}")
            return False
        if not tpm.acquire(estimated_tokens):
            logger.warning(f"Rate limited (TPM): {model}")
            return False
        return True

    def wait_and_acquire(
        self, model: str, estimated_tokens: int = 1, timeout: float = 30.0
    ) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.acquire(model, estimated_tokens):
                return True
            time.sleep(0.1)
        return False
