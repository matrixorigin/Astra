"""Rate limiting middleware — per-API-key throttle.

v1: in-memory sliding window. v2: swap to Redis for distributed.
"""

import time
from collections import defaultdict

from fastapi import Request, HTTPException, status
from starlette.middleware.base import BaseHTTPMiddleware

# Limits: (max_requests, window_seconds)
_RATE_LIMITS: dict[str, tuple[int, int]] = {
    # Write operations
    "POST:/v1/memories": (300, 60),
    "POST:/v1/memories/batch": (60, 60),
    "PUT:/v1/memories/": (120, 60),
    "DELETE:/v1/memories/": (120, 60),
    "POST:/v1/memories/purge": (30, 60),
    "POST:/v1/observe": (120, 60),
    # Read operations
    "POST:/v1/memories/retrieve": (600, 60),
    "POST:/v1/memories/search": (600, 60),
    "GET:/v1/memories": (300, 60),
    "GET:/v1/profiles/": (120, 60),
    # Expensive operations
    "POST:/v1/consolidate": (3, 3600),
    "POST:/v1/reflect": (2, 7200),
    # Snapshots
    "POST:/v1/snapshots": (30, 60),
    "GET:/v1/snapshots": (120, 60),
    "DELETE:/v1/snapshots/": (30, 60),
    # Auth
    "POST:/auth/keys": (20, 60),
    # Global fallback
    "_default": (1000, 60),
}


class _SlidingWindow:
    __slots__ = ("timestamps",)

    def __init__(self):
        self.timestamps: list[float] = []

    def hit(self, now: float, window: int) -> int:
        cutoff = now - window
        self.timestamps = [t for t in self.timestamps if t > cutoff]
        self.timestamps.append(now)
        return len(self.timestamps)


# key → (method:path_prefix) → SlidingWindow
_windows: dict[str, dict[str, _SlidingWindow]] = defaultdict(lambda: defaultdict(_SlidingWindow))


def _match_limit(method: str, path: str) -> tuple[int, int]:
    """Find the most specific rate limit for this request."""
    # Exact match first
    key = f"{method}:{path}"
    if key in _RATE_LIMITS:
        return _RATE_LIMITS[key]
    # Prefix match (for parameterized paths like /v1/memories/{id})
    for pattern, limit in _RATE_LIMITS.items():
        if pattern == "_default":
            continue
        p_method, p_path = pattern.split(":", 1)
        if method == p_method and path.startswith(p_path):
            return limit
    return _RATE_LIMITS["_default"]


class RateLimitMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next):
        # Extract API key from Authorization header
        auth = request.headers.get("authorization", "")
        if not auth.startswith("Bearer "):
            return await call_next(request)

        api_key = auth[7:]  # Use raw key as identity (hashed in prod)
        key_id = api_key[:12]  # prefix only, don't store full key

        method = request.method
        path = request.url.path
        max_req, window = _match_limit(method, path)

        now = time.time()
        route_key = f"{method}:{path.split('?')[0]}"
        count = _windows[key_id][route_key].hit(now, window)

        if count > max_req:
            from starlette.responses import JSONResponse
            return JSONResponse(
                status_code=429,
                content={"detail": f"Rate limit exceeded. Max {max_req} requests per {window}s."},
                headers={"Retry-After": str(window)},
            )

        response = await call_next(request)
        response.headers["X-RateLimit-Limit"] = str(max_req)
        response.headers["X-RateLimit-Remaining"] = str(max(0, max_req - count))
        return response
