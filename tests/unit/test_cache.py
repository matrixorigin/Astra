"""Unit tests for core/cache.py — RedisCache and cached decorator."""

import json
from unittest.mock import MagicMock, patch

import pytest

from core.cache import RedisCache, cached, get_cache


@pytest.fixture(autouse=True)
def reset_global_cache():
    import core.cache as mod

    mod._cache = None
    yield
    mod._cache = None


@pytest.fixture
def mock_redis_client():
    client = MagicMock()
    client.ping.return_value = True
    return client


@pytest.fixture
def cache(mock_redis_client):
    with patch("core.cache.redis.from_url", return_value=mock_redis_client):
        with patch.dict("os.environ", {"CACHE_ENABLED": "true"}):
            c = RedisCache()
    c.client = mock_redis_client
    return c


class TestRedisCacheDisabled:
    def test_disabled_get_returns_none(self):
        with patch.dict("os.environ", {"CACHE_ENABLED": "false"}):
            c = RedisCache()
        assert c.get("key") is None

    def test_disabled_set_noop(self):
        with patch.dict("os.environ", {"CACHE_ENABLED": "false"}):
            c = RedisCache()
        c.set("key", "value")  # should not raise

    def test_disabled_delete_noop(self):
        with patch.dict("os.environ", {"CACHE_ENABLED": "false"}):
            c = RedisCache()
        c.delete("key")

    def test_disabled_clear_pattern_noop(self):
        with patch.dict("os.environ", {"CACHE_ENABLED": "false"}):
            c = RedisCache()
        c.clear_pattern("*")

    def test_connection_failure_disables_cache(self):
        mock_client = MagicMock()
        mock_client.ping.side_effect = Exception("connection refused")
        with patch("core.cache.redis.from_url", return_value=mock_client):
            with patch.dict("os.environ", {"CACHE_ENABLED": "true"}):
                c = RedisCache()
        assert c.enabled is False


class TestRedisCacheEnabled:
    def test_get_hit(self, cache, mock_redis_client):
        mock_redis_client.get.return_value = json.dumps({"x": 1})
        result = cache.get("mykey")
        assert result == {"x": 1}

    def test_get_miss(self, cache, mock_redis_client):
        mock_redis_client.get.return_value = None
        assert cache.get("missing") is None

    def test_get_error_returns_none(self, cache, mock_redis_client):
        mock_redis_client.get.side_effect = Exception("redis error")
        assert cache.get("key") is None

    def test_set_calls_setex(self, cache, mock_redis_client):
        cache.set("k", {"v": 2}, ttl=60)
        mock_redis_client.setex.assert_called_once_with("k", 60, json.dumps({"v": 2}))

    def test_set_error_does_not_raise(self, cache, mock_redis_client):
        mock_redis_client.setex.side_effect = Exception("redis error")
        cache.set("k", "v")  # should not raise

    def test_delete(self, cache, mock_redis_client):
        cache.delete("k")
        mock_redis_client.delete.assert_called_once_with("k")

    def test_delete_error_does_not_raise(self, cache, mock_redis_client):
        mock_redis_client.delete.side_effect = Exception("redis error")
        cache.delete("k")

    def test_clear_pattern(self, cache, mock_redis_client):
        mock_redis_client.keys.return_value = ["a", "b"]
        cache.clear_pattern("prefix:*")
        mock_redis_client.delete.assert_called_once_with("a", "b")

    def test_clear_pattern_no_keys(self, cache, mock_redis_client):
        mock_redis_client.keys.return_value = []
        cache.clear_pattern("prefix:*")
        mock_redis_client.delete.assert_not_called()

    def test_clear_pattern_error_does_not_raise(self, cache, mock_redis_client):
        mock_redis_client.keys.side_effect = Exception("redis error")
        cache.clear_pattern("*")


class TestGetCache:
    def test_returns_singleton(self):
        with patch("core.cache.redis.from_url") as mock_from_url:
            mock_from_url.return_value = MagicMock()
            mock_from_url.return_value.ping.side_effect = Exception("no redis")
            c1 = get_cache()
            c2 = get_cache()
        assert c1 is c2


class TestCachedDecorator:
    def test_cache_miss_calls_function(self):
        mock_client = MagicMock()
        mock_client.ping.side_effect = Exception("no redis")
        with patch("core.cache.redis.from_url", return_value=mock_client):
            calls = []

            @cached(ttl=60, key_prefix="test")
            def fn(x):
                calls.append(x)
                return x * 2

            result = fn(3)
        assert result == 6
        assert calls == [3]

    def test_cache_hit_skips_function(self):
        mock_client = MagicMock()
        mock_client.ping.return_value = True
        mock_client.get.return_value = json.dumps(42)

        with patch("core.cache.redis.from_url", return_value=mock_client):
            calls = []

            @cached(ttl=60, key_prefix="test")
            def fn(x):
                calls.append(x)
                return x * 2

            result = fn(3)
        assert result == 42
        assert calls == []
