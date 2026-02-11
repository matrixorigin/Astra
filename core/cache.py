"""Redis cache layer for performance optimization."""

import json
import os
from functools import wraps
from typing import Any

import redis

from core.logging_config import get_logger

logger = get_logger(__name__)


class RedisCache:
    """Redis cache client."""

    def __init__(self):
        """Initialize Redis connection."""
        redis_url = os.getenv("REDIS_URL", "redis://localhost:6379/0")
        self.enabled = os.getenv("CACHE_ENABLED", "true").lower() == "true"

        if self.enabled:
            try:
                self.client = redis.from_url(redis_url, decode_responses=True)
                self.client.ping()
                logger.info(f"Redis cache connected: {redis_url}")
            except Exception as e:
                logger.warning(f"Redis connection failed, caching disabled: {e}")
                self.enabled = False
        else:
            logger.info("Redis cache disabled")

    def get(self, key: str) -> Any | None:
        """Get value from cache."""
        if not self.enabled:
            return None

        try:
            value = self.client.get(key)
            if value:
                return json.loads(value)
        except Exception as e:
            logger.error(f"Cache get error: {e}")

        return None

    def set(self, key: str, value: Any, ttl: int = 300):
        """Set value in cache with TTL (seconds)."""
        if not self.enabled:
            return

        try:
            self.client.setex(key, ttl, json.dumps(value))
        except Exception as e:
            logger.error(f"Cache set error: {e}")

    def delete(self, key: str):
        """Delete key from cache."""
        if not self.enabled:
            return

        try:
            self.client.delete(key)
        except Exception as e:
            logger.error(f"Cache delete error: {e}")

    def clear_pattern(self, pattern: str):
        """Clear all keys matching pattern."""
        if not self.enabled:
            return

        try:
            keys = self.client.keys(pattern)
            if keys:
                self.client.delete(*keys)
                logger.info(f"Cleared {len(keys)} cache keys matching {pattern}")
        except Exception as e:
            logger.error(f"Cache clear error: {e}")


# Global cache instance
_cache = None


def get_cache():
    """Get global cache instance."""
    global _cache
    if _cache is None:
        _cache = RedisCache()
    return _cache


def cached(ttl: int = 300, key_prefix: str = ""):
    """Decorator for caching function results.

    Args:
        ttl: Time to live in seconds
        key_prefix: Prefix for cache key
    """

    def decorator(func):
        @wraps(func)
        def wrapper(*args, **kwargs):
            cache = get_cache()

            # Generate cache key
            key_parts = [key_prefix or func.__name__]
            key_parts.extend(str(arg) for arg in args)
            key_parts.extend(f"{k}={v}" for k, v in sorted(kwargs.items()))
            cache_key = ":".join(key_parts)

            # Try cache
            result = cache.get(cache_key)
            if result is not None:
                logger.debug(f"Cache hit: {cache_key}")
                return result

            # Execute function
            result = func(*args, **kwargs)

            # Store in cache
            cache.set(cache_key, result, ttl)
            logger.debug(f"Cache miss: {cache_key}")

            return result

        return wrapper

    return decorator
