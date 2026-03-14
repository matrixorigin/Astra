"""Unit tests for core/rate_limit.py and core/events/causal_chain.py."""

import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from unittest.mock import patch as _patch

# Patch settings before importing module-level rate_limiter
with _patch("config.settings.get_settings") as _mock_settings:
    _mock_settings.return_value.security.rate_limit_per_minute = 60
    from core.rate_limit import RateLimiter


class TestRateLimiter:
    @pytest.fixture
    def limiter(self):
        return RateLimiter(requests_per_minute=5)

    def test_allows_within_limit(self, limiter):
        allowed, remaining = limiter.check_rate_limit("user1")
        assert allowed is True
        assert remaining == 4

    def test_blocks_over_limit(self, limiter):
        for _ in range(5):
            limiter.check_rate_limit("user2")
        allowed, remaining = limiter.check_rate_limit("user2")
        assert allowed is False
        assert remaining == 0

    def test_different_keys_independent(self, limiter):
        for _ in range(5):
            limiter.check_rate_limit("userA")
        allowed, _ = limiter.check_rate_limit("userB")
        assert allowed is True

    def test_cleans_old_requests(self, limiter):
        # Inject old timestamps directly
        old_time = time.time() - 61
        limiter.requests["user3"] = [old_time] * 5
        allowed, _ = limiter.check_rate_limit("user3")
        assert allowed is True

    @pytest.mark.asyncio
    async def test_middleware_allowed(self, limiter):
        request = MagicMock()
        request.state.user_id = "u1"
        response = MagicMock()
        response.headers = {}
        call_next = AsyncMock(return_value=response)

        result = await limiter(request, call_next)
        assert result is response
        assert "X-RateLimit-Limit" in result.headers

    @pytest.mark.asyncio
    async def test_middleware_blocked(self, limiter):
        from fastapi import HTTPException

        request = MagicMock()
        request.state.user_id = "u_block"
        call_next = AsyncMock()

        for _ in range(5):
            limiter.check_rate_limit("u_block")

        with pytest.raises(HTTPException) as exc:
            await limiter(request, call_next)
        assert exc.value.status_code == 429

    @pytest.mark.asyncio
    async def test_middleware_uses_ip_when_no_user(self, limiter):
        request = MagicMock()
        request.state.user_id = None
        request.client.host = "1.2.3.4"
        response = MagicMock()
        response.headers = {}
        call_next = AsyncMock(return_value=response)

        result = await limiter(request, call_next)
        assert result is response


class TestCausalChainManager:
    @pytest.fixture
    def manager(self):
        from core.events.causal_chain import CausalChainManager

        mock_factory = MagicMock()
        mock_db = MagicMock()
        mock_factory.return_value.__enter__ = MagicMock(return_value=mock_db)
        mock_factory.return_value.__exit__ = MagicMock(return_value=False)
        mgr = CausalChainManager(mock_factory)
        mgr._mock_db = mock_db
        return mgr

    def _make_event(self, event_id, parent_id=None, chain_id="chain1", event_type="user_query"):
        from core.events.models import ConversationEvent

        e = MagicMock(spec=ConversationEvent)
        e.event_id = event_id
        e.parent_event_id = parent_id
        e.causal_chain_id = chain_id
        e.event_type = event_type
        e.token_usage = None
        e.created_at = "2026-01-01"
        return e

    def test_get_chain_delegates_to_reader(self, manager):
        events = [self._make_event("e1"), self._make_event("e2", parent_id="e1")]
        manager.reader.get_causal_chain = MagicMock(return_value=events)
        result = manager.get_chain("chain1")
        assert result == events

    def test_get_parent_event_no_parent(self, manager):
        e = self._make_event("e1", parent_id=None)
        manager.reader.get_event = MagicMock(return_value=e)
        assert manager.get_parent_event("e1") is None

    def test_get_parent_event_with_parent(self, manager):
        child = self._make_event("e2", parent_id="e1")
        parent = self._make_event("e1")
        manager.reader.get_event = MagicMock(
            side_effect=lambda eid: child if eid == "e2" else parent
        )
        result = manager.get_parent_event("e2")
        assert result is parent

    def test_get_parent_event_not_found(self, manager):
        manager.reader.get_event = MagicMock(return_value=None)
        assert manager.get_parent_event("missing") is None

    def test_get_chain_summary_empty(self, manager):
        manager.reader.get_causal_chain = MagicMock(return_value=[])
        summary = manager.get_chain_summary("chain1")
        assert summary["total_events"] == 0
        assert summary["total_tokens"] == 0
        assert summary["first_event_at"] is None

    def test_get_chain_summary_with_events(self, manager):
        e1 = self._make_event("e1", event_type="user_query")
        e2 = self._make_event("e2", event_type="llm_response")
        manager.reader.get_causal_chain = MagicMock(return_value=[e1, e2])
        summary = manager.get_chain_summary("chain1")
        assert summary["total_events"] == 2
        assert summary["event_types"]["user_query"] == 1
        assert summary["event_types"]["llm_response"] == 1

    def test_validate_chain_integrity_valid(self, manager):
        e1 = self._make_event("e1")
        e2 = self._make_event("e2", parent_id="e1")
        manager.reader.get_causal_chain = MagicMock(return_value=[e1, e2])
        result = manager.validate_chain_integrity("chain1")
        assert result["is_valid"] is True
        assert result["issues"] == []

    def test_validate_chain_integrity_broken(self, manager):
        e2 = self._make_event("e2", parent_id="missing_parent")
        manager.reader.get_causal_chain = MagicMock(return_value=[e2])
        result = manager.validate_chain_integrity("chain1")
        assert result["is_valid"] is False
        assert len(result["issues"]) == 1
