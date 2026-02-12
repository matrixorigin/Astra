"""Unit tests for ContextManager with caching functionality."""

import os
from unittest.mock import MagicMock, patch

import pytest

from core.context.manager import Context, ContextFragment, ContextManager, TaskType


class MockCache:
    """Mock cache with in-memory storage."""

    def __init__(self):
        self._cache = {}

    def get(self, key):
        return self._cache.get(key)

    def set(self, key, value, ttl=300):
        self._cache[key] = value

    def delete(self, key):
        if key in self._cache:
            del self._cache[key]

    def clear_pattern(self, pattern):
        # Simple pattern matching for test
        keys_to_delete = [k for k in self._cache if pattern.replace("*", "") in k]
        for k in keys_to_delete:
            del self._cache[k]

    def clear(self):
        self._cache.clear()


@pytest.fixture
def mock_db():
    """Mock database connection."""
    db = MagicMock()
    db.fetchone.return_value = None
    db.fetchall.return_value = []
    return db


@pytest.fixture
def context_manager(mock_db):
    """Create ContextManager with mocked dependencies."""
    mock_cache = MockCache()

    with patch("core.context.manager.get_cache", return_value=mock_cache):
        # Mock the imports inside __init__
        with patch("core.context.embeddings.EmbeddingService"):
            with patch("core.context.prompts.PromptManager"):
                with patch("core.context.scorer.RelevanceScorer"):
                    # Set environment variables for testing
                    with patch.dict(
                        os.environ, {"CONTEXT_VERSION": "test", "CONTEXT_CACHE_TTL": "60"}
                    ):
                        cm = ContextManager(db=mock_db, embedding_provider="mock")
                        # Override cache with mock
                        cm.cache = mock_cache
                        cm.context_version = "test"
                        cm.cache_ttl = 60
                        yield cm


class TestContextManagerInit:
    """Tests for ContextManager initialization."""

    def test_initialization_with_defaults(self, mock_db):
        """Test ContextManager initializes with default values."""
        with patch("core.context.manager.get_cache"):
            with patch("core.context.embeddings.EmbeddingService"):
                with patch("core.context.prompts.PromptManager"):
                    with patch("core.context.scorer.RelevanceScorer"):
                        with patch.dict("os.environ", {}, clear=True):
                            cm = ContextManager(db=mock_db)

                            assert cm.db == mock_db
                            assert cm.context_version == "v1"
                            assert cm.cache_ttl == 300

    def test_initialization_with_custom_ttl(self, mock_db):
        """Test ContextManager uses custom TTL from environment."""
        with patch("core.context.manager.get_cache"):
            with patch("core.context.embeddings.EmbeddingService"):
                with patch("core.context.prompts.PromptManager"):
                    with patch("core.context.scorer.RelevanceScorer"):
                        with patch.dict("os.environ", {"CONTEXT_CACHE_TTL": "120"}):
                            cm = ContextManager(db=mock_db)
                            assert cm.cache_ttl == 120

    def test_initialization_with_custom_version(self, mock_db):
        """Test ContextManager uses custom version from environment."""
        with patch("core.context.manager.get_cache"):
            with patch("core.context.embeddings.EmbeddingService"):
                with patch("core.context.prompts.PromptManager"):
                    with patch("core.context.scorer.RelevanceScorer"):
                        with patch.dict("os.environ", {"CONTEXT_VERSION": "v2"}):
                            cm = ContextManager(db=mock_db)
                            assert cm.context_version == "v2"


class TestHashQuery:
    """Tests for _hash_query method."""

    def test_hash_query_deterministic(self, context_manager):
        """Test hashing is deterministic for same input."""
        query = "test query"
        hash1 = context_manager._hash_query(query)
        hash2 = context_manager._hash_query(query)

        assert hash1 == hash2
        assert len(hash1) == 64  # SHA256 hex digest length

    def test_hash_query_different_inputs(self, context_manager):
        """Test different queries produce different hashes."""
        hash1 = context_manager._hash_query("query1")
        hash2 = context_manager._hash_query("query2")

        assert hash1 != hash2

    def test_hash_query_empty_string(self, context_manager):
        """Test hashing empty string."""
        hash_result = context_manager._hash_query("")
        assert len(hash_result) == 64

    def test_hash_query_unicode(self, context_manager):
        """Test hashing unicode characters."""
        query = "测试查询 🚀"
        hash_result = context_manager._hash_query(query)
        assert len(hash_result) == 64


class TestGetCacheKey:
    """Tests for _get_cache_key method."""

    def test_cache_key_format(self, context_manager):
        """Test cache key has correct format."""
        key = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id="event456",
        )

        assert key.startswith("context:test:session123:")
        assert "general" in key
        assert "8000" in key
        assert "event456" in key

    def test_cache_key_with_none_event_id(self, context_manager):
        """Test cache key handles None event_id."""
        key = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id=None,
        )

        assert "none" in key

    def test_cache_key_different_task_types(self, context_manager):
        """Test cache key varies by task type."""
        key1 = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.CODE_REVIEW,
            max_tokens=8000,
            last_event_id="event456",
        )

        key2 = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.PLANNING,
            max_tokens=8000,
            last_event_id="event456",
        )

        assert key1 != key2
        assert "code_review" in key1
        assert "planning" in key2

    def test_cache_key_different_queries(self, context_manager):
        """Test cache key varies by query hash."""
        key1 = context_manager._get_cache_key(
            session_id="session123",
            query="query one",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id="event456",
        )

        key2 = context_manager._get_cache_key(
            session_id="session123",
            query="query two",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id="event456",
        )

        assert key1 != key2


class TestGetLastEventId:
    """Tests for _get_last_event_id method."""

    def test_get_last_event_id_exists(self, context_manager, mock_db):
        """Test getting last event ID when it exists."""
        mock_db.fetchone.return_value = {"event_id": "event_123"}

        result = context_manager._get_last_event_id("session123")

        assert result == "event_123"
        mock_db.fetchone.assert_called_once()

    def test_get_last_event_id_none(self, context_manager, mock_db):
        """Test getting last event ID when no events exist."""
        mock_db.fetchone.return_value = None

        result = context_manager._get_last_event_id("session123")

        assert result is None


class TestGetCachedContext:
    """Tests for _get_cached_context method."""

    def test_get_cached_context_hit(self, context_manager):
        """Test retrieving cached context."""
        context = Context(
            system_prompt="test prompt",
            skill_definitions=[{"name": "test"}],
            selected_events=[{"event_id": "e1"}],
            code_context=[],
            documentation=[],
            total_tokens=100,
            token_budget={"system": 500},
            assembly_time_ms=50,
            relevance_scores={"e1": 0.9},
            task_type=TaskType.GENERAL,
        )

        cache_key = "test:key"
        context_manager._set_cached_context(cache_key, context)

        cached = context_manager._get_cached_context(cache_key)

        assert cached is not None
        assert cached.system_prompt == "test prompt"
        assert cached.total_tokens == 100

    def test_get_cached_context_miss(self, context_manager):
        """Test cache miss returns None."""
        result = context_manager._get_cached_context("nonexistent:key")
        assert result is None

    def test_get_cached_context_invalid_data(self, context_manager):
        """Test invalid cached data returns None."""
        # Store invalid data
        context_manager.cache.set("invalid:key", {"invalid": "data"})

        result = context_manager._get_cached_context("invalid:key")
        assert result is None


class TestSetCachedContext:
    """Tests for _set_cached_context method."""

    def test_set_cached_context(self, context_manager):
        """Test storing context in cache."""
        context = Context(
            system_prompt="test prompt",
            skill_definitions=[{"name": "test"}],
            selected_events=[{"event_id": "e1"}],
            code_context=[],
            documentation=[],
            total_tokens=100,
            token_budget={"system": 500},
            assembly_time_ms=50,
            relevance_scores={"e1": 0.9},
            task_type=TaskType.GENERAL,
        )

        cache_key = "test:key"
        context_manager._set_cached_context(cache_key, context)

        cached = context_manager.cache.get(cache_key)
        assert cached is not None
        assert cached["system_prompt"] == "test prompt"
        assert cached["task_type"] == "general"

    def test_set_cached_context_preserves_all_fields(self, context_manager):
        """Test all context fields are preserved in cache."""
        context = Context(
            system_prompt="test prompt",
            skill_definitions=[{"name": "skill1", "desc": "desc1"}],
            selected_events=[{"event_id": "e1", "content": "test"}],
            code_context=[{"file": "test.py", "content": "code"}],
            documentation=[{"content": "doc"}],
            total_tokens=200,
            token_budget={"system": 500, "history": 1000},
            assembly_time_ms=100,
            relevance_scores={"e1": 0.95},
            task_type=TaskType.CODE_REVIEW,
        )

        cache_key = "test:key"
        context_manager._set_cached_context(cache_key, context)

        cached = context_manager.cache.get(cache_key)
        assert cached["skill_definitions"] == [{"name": "skill1", "desc": "desc1"}]
        assert cached["code_context"] == [{"file": "test.py", "content": "code"}]
        assert cached["total_tokens"] == 200
        assert cached["assembly_time_ms"] == 100


class TestInvalidateSessionCache:
    """Tests for invalidate_session_cache method."""

    def test_invalidate_session_cache(self, context_manager):
        """Test invalidating cache for a session."""
        # Add some cache entries
        context_manager._set_cached_context(
            "context:test:session1:general:8000:e1:hash1",
            Context(
                system_prompt="p1",
                skill_definitions=[],
                selected_events=[],
                code_context=[],
                documentation=[],
                total_tokens=100,
                token_budget={},
                assembly_time_ms=10,
                relevance_scores={},
                task_type=TaskType.GENERAL,
            ),
        )
        context_manager._set_cached_context(
            "context:test:session1:planning:8000:e2:hash2",
            Context(
                system_prompt="p2",
                skill_definitions=[],
                selected_events=[],
                code_context=[],
                documentation=[],
                total_tokens=100,
                token_budget={},
                assembly_time_ms=10,
                relevance_scores={},
                task_type=TaskType.PLANNING,
            ),
        )
        context_manager._set_cached_context(
            "context:test:session2:general:8000:e3:hash3",
            Context(
                system_prompt="p3",
                skill_definitions=[],
                selected_events=[],
                code_context=[],
                documentation=[],
                total_tokens=100,
                token_budget={},
                assembly_time_ms=10,
                relevance_scores={},
                task_type=TaskType.GENERAL,
            ),
        )

        # Invalidate session1
        context_manager.invalidate_session_cache("session1")

        # session1 entries should be gone
        assert context_manager.cache.get("context:test:session1:general:8000:e1:hash1") is None
        assert context_manager.cache.get("context:test:session1:planning:8000:e2:hash2") is None
        # session2 entry should remain
        assert context_manager.cache.get("context:test:session2:general:8000:e3:hash3") is not None

    def test_invalidate_session_cache_no_match(self, context_manager):
        """Test invalidating cache for non-existent session."""
        # Add some cache entries
        context_manager._set_cached_context(
            "context:test:session1:general:8000:e1:hash1",
            Context(
                system_prompt="p1",
                skill_definitions=[],
                selected_events=[],
                code_context=[],
                documentation=[],
                total_tokens=100,
                token_budget={},
                assembly_time_ms=10,
                relevance_scores={},
                task_type=TaskType.GENERAL,
            ),
        )

        # Try to invalidate non-existent session
        context_manager.invalidate_session_cache("nonexistent")

        # Original entry should remain
        assert context_manager.cache.get("context:test:session1:general:8000:e1:hash1") is not None


class TestBuildContext:
    """Tests for build_context method."""

    def test_build_context_cache_hit(self, context_manager):
        """Test build_context returns cached result when available."""
        # Pre-populate cache
        cached_context = Context(
            system_prompt="cached prompt",
            skill_definitions=[],
            selected_events=[],
            code_context=[],
            documentation=[],
            total_tokens=100,
            token_budget={},
            assembly_time_ms=10,
            relevance_scores={},
            task_type=TaskType.GENERAL,
        )
        cache_key = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id=None,
        )
        context_manager._set_cached_context(cache_key, cached_context)

        # Mock database to return no events (should not be called due to cache hit)
        context_manager.db.fetchone.return_value = None
        context_manager.db.fetchall.return_value = []

        result = context_manager.build_context(
            session_id="session123",
            query="test query",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
        )

        assert result.system_prompt == "cached prompt"
        # Database should not be called for candidates when cache hit
        # (Note: _get_last_event_id is still called)

    def test_build_context_cache_miss(self, context_manager):
        """Test build_context builds new context when cache miss."""
        # Clear any existing cache
        context_manager.cache.clear()

        # Mock database responses
        context_manager.db.fetchone.return_value = None  # No last event
        context_manager.db.fetchall.return_value = [
            {
                "event_id": "e1",
                "event_type": "user_query",
                "content": "test content",
                "created_at": "2024-01-01",
                "parent_event_id": None,
                "causal_chain_id": "chain1",
                "metadata": {},
            }
        ]

        # Mock scorer to return high scores
        with patch.object(context_manager.scorer, "score_candidates") as mock_score:
            mock_score.return_value = [
                ({"event_id": "e1", "content": "test"}, 0.9, {}),
            ]

        # Mock skill definitions to avoid DB query
        with patch.object(context_manager, "_get_skill_definitions", return_value=[]):
            result = context_manager.build_context(
                session_id="session123",
                query="test query",
                task_type=TaskType.GENERAL,
                max_tokens=8000,
            )

        assert result is not None
        assert result.total_tokens >= 0
        cache_key = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id=None,
        )
        assert context_manager.cache.get(cache_key) is not None

    def test_build_context_with_refresh(self, context_manager):
        """Test build_context with refresh=True bypasses cache."""
        # Pre-populate cache
        cached_context = Context(
            system_prompt="old prompt",
            skill_definitions=[],
            selected_events=[],
            code_context=[],
            documentation=[],
            total_tokens=100,
            token_budget={},
            assembly_time_ms=10,
            relevance_scores={},
            task_type=TaskType.GENERAL,
        )
        cache_key = context_manager._get_cache_key(
            session_id="session123",
            query="test query",
            task_type=TaskType.GENERAL,
            max_tokens=8000,
            last_event_id=None,
        )
        context_manager._set_cached_context(cache_key, cached_context)

        # Mock database responses for new context
        context_manager.db.fetchone.return_value = None
        context_manager.db.fetchall.return_value = [
            {
                "event_id": "e1",
                "event_type": "user_query",
                "content": "new content",
                "created_at": "2024-01-01",
                "parent_event_id": None,
                "causal_chain_id": "chain1",
                "metadata": {},
            }
        ]

        with patch.object(context_manager.scorer, "score_candidates") as mock_score:
            mock_score.return_value = [
                ({"event_id": "e1", "content": "new content"}, 0.95, {}),
            ]

        # Mock skill definitions to avoid DB query
        with patch.object(context_manager, "_get_skill_definitions", return_value=[]):
            # Build with refresh=True
            result = context_manager.build_context(
                session_id="session123",
                query="test query",
                task_type=TaskType.GENERAL,
                max_tokens=8000,
                refresh=True,
            )

        # Should get new context, not cached one
        assert result.system_prompt != "old prompt"
        refreshed_cache = context_manager.cache.get(cache_key)
        assert refreshed_cache is not None
        assert refreshed_cache["system_prompt"] != "old prompt"


class TestContextDataclass:
    """Tests for Context dataclass."""

    def test_context_creation(self):
        """Test Context dataclass creation."""
        context = Context(
            system_prompt="test prompt",
            skill_definitions=[{"name": "test"}],
            selected_events=[{"event_id": "e1"}],
            code_context=[],
            documentation=[],
            total_tokens=100,
            token_budget={"system": 500},
            assembly_time_ms=50,
            relevance_scores={"e1": 0.9},
            task_type=TaskType.GENERAL,
        )

        assert context.system_prompt == "test prompt"
        assert context.total_tokens == 100
        assert context.task_type == TaskType.GENERAL

    def test_context_to_prompt(self):
        """Test Context to_prompt method."""
        context = Context(
            system_prompt="You are a helpful assistant.",
            skill_definitions=[{"name": "test_skill", "description": "A test skill"}],
            selected_events=[{"event_type": "user_query", "content": "Hello"}],
            code_context=[{"file": "test.py", "content": "print('hello')"}],
            documentation=[{"content": "Documentation content"}],
            total_tokens=100,
            token_budget={"system": 500},
            assembly_time_ms=50,
            relevance_scores={"e1": 0.9},
            task_type=TaskType.GENERAL,
        )

        prompt = context.to_prompt()

        assert "You are a helpful assistant." in prompt
        assert "test_skill" in prompt
        assert "Hello" in prompt
        assert "test.py" in prompt
        assert "Documentation content" in prompt

    def test_context_to_prompt_empty(self):
        """Test Context to_prompt with minimal data."""
        context = Context(
            system_prompt="Default prompt",
            skill_definitions=[],
            selected_events=[],
            code_context=[],
            documentation=[],
            total_tokens=0,
            token_budget={},
            assembly_time_ms=0,
            relevance_scores={},
            task_type=TaskType.GENERAL,
        )

        prompt = context.to_prompt()
        assert "Default prompt" in prompt


class TestTaskType:
    """Tests for TaskType enum."""

    def test_task_type_values(self):
        """Test TaskType has expected values."""
        assert TaskType.CODE_REVIEW.value == "code_review"
        assert TaskType.PLANNING.value == "planning"
        assert TaskType.DEBUGGING.value == "debugging"
        assert TaskType.GENERAL.value == "general"

    def test_task_type_from_string(self):
        """Test creating TaskType from string."""
        assert TaskType("code_review") == TaskType.CODE_REVIEW
        assert TaskType("planning") == TaskType.PLANNING


class TestContextFragment:
    """Tests for ContextFragment dataclass."""

    def test_context_fragment_creation(self):
        """Test ContextFragment dataclass creation."""
        fragment = ContextFragment(
            content="test content",
            tokens=10,
            source="event",
            relevance_score=0.8,
            metadata={"key": "value"},
        )

        assert fragment.content == "test content"
        assert fragment.tokens == 10
        assert fragment.source == "event"
        assert fragment.relevance_score == 0.8
        assert fragment.metadata == {"key": "value"}



class TestAllocateBudget:
    """Test token budget allocation."""

    def test_code_review_budget(self, context_manager):
        """Test CODE_REVIEW budget allocation (60% code, 20% history, 20% docs)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.CODE_REVIEW)

        # Fixed allocations
        assert budget["system"] == 500
        assert budget["skills"] == 1000
        assert budget["reserve"] == 500

        # Dynamic allocations (10000 - 2000 = 8000 available)
        available = 8000
        assert budget["code"] == int(available * 0.6)  # 4800
        assert budget["history"] == int(available * 0.2)  # 1600
        assert budget["docs"] == int(available * 0.2)  # 1600

    def test_planning_budget(self, context_manager):
        """Test PLANNING budget allocation (60% history, 20% code, 20% docs)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.PLANNING)

        available = 8000
        assert budget["history"] == int(available * 0.6)  # 4800
        assert budget["code"] == int(available * 0.2)  # 1600
        assert budget["docs"] == int(available * 0.2)  # 1600

    def test_debugging_budget(self, context_manager):
        """Test DEBUGGING budget allocation (40% code, 40% logs, 20% history)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.DEBUGGING)

        available = 8000
        assert budget["code"] == int(available * 0.4)  # 3200
        assert budget["docs"] == int(available * 0.2)  # 1600 (logs as docs)
        assert budget["history"] == int(available * 0.2)  # 1600

    def test_general_budget(self, context_manager):
        """Test GENERAL budget allocation (50% history, 30% code, 20% docs)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.GENERAL)

        available = 8000
        assert budget["history"] == int(available * 0.5)  # 4000
        assert budget["code"] == int(available * 0.3)  # 2400
        assert budget["docs"] == int(available * 0.2)  # 1600

    def test_budget_with_small_total(self, context_manager):
        """Test budget allocation with small total tokens."""
        total_tokens = 2000  # Less than fixed allocations
        budget = context_manager._allocate_budget(total_tokens, TaskType.GENERAL)

        # Fixed allocations remain
        assert budget["system"] == 500
        assert budget["skills"] == 1000
        assert budget["reserve"] == 500

        # Dynamic allocations should be 0 or minimal
        assert budget["history"] >= 0
        assert budget["code"] >= 0
        assert budget["docs"] >= 0
