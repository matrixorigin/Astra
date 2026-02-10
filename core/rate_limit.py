"""Rate limiting middleware."""

import time
from typing import Dict, Tuple
from fastapi import Request, HTTPException, status
from collections import defaultdict

from core.config import get_settings
from core.logging_config import get_logger

settings = get_settings()
logger = get_logger(__name__)


class RateLimiter:
    """Simple in-memory rate limiter.
    
    In production, use Redis for distributed rate limiting.
    """

    def __init__(self, requests_per_minute: int = None):
        self.requests_per_minute = requests_per_minute or settings.security.rate_limit_per_minute
        self.requests: Dict[str, list[float]] = defaultdict(list)

    def _clean_old_requests(self, key: str, now: float) -> None:
        """Remove requests older than 1 minute."""
        cutoff = now - 60
        self.requests[key] = [req_time for req_time in self.requests[key] if req_time > cutoff]

    def check_rate_limit(self, key: str) -> Tuple[bool, int]:
        """Check if request is within rate limit.
        
        Args:
            key: Rate limit key (e.g., user_id or IP)
            
        Returns:
            Tuple of (allowed, remaining_requests)
        """
        now = time.time()
        
        # Clean old requests
        self._clean_old_requests(key, now)
        
        # Check limit
        request_count = len(self.requests[key])
        
        if request_count >= self.requests_per_minute:
            logger.warning(f"Rate limit exceeded for key: {key}")
            return False, 0
        
        # Add current request
        self.requests[key].append(now)
        remaining = self.requests_per_minute - request_count - 1
        
        return True, remaining

    async def __call__(self, request: Request, call_next):
        """Rate limiting middleware."""
        # Get rate limit key (user_id or IP)
        user_id = getattr(request.state, "user_id", None)
        key = user_id or request.client.host
        
        # Check rate limit
        allowed, remaining = self.check_rate_limit(key)
        
        if not allowed:
            logger.warning(f"Rate limit exceeded for {key}")
            raise HTTPException(
                status_code=status.HTTP_429_TOO_MANY_REQUESTS,
                detail="Rate limit exceeded. Please try again later.",
                headers={"Retry-After": "60"},
            )
        
        # Add rate limit headers
        response = await call_next(request)
        response.headers["X-RateLimit-Limit"] = str(self.requests_per_minute)
        response.headers["X-RateLimit-Remaining"] = str(remaining)
        
        return response


# Global rate limiter instance
rate_limiter = RateLimiter()
