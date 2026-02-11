"""Integration test for scope-based configuration."""

import pytest

from core.llm.client import LLMClient
from core.scope.scope_resolver import ScopeChainBuilder, ScopeResolver


class MockDB:
    """Mock database with scope-based configs."""

    def __init__(self):
        self.tokens = [
            # Global token
            {
                "token_type": "llm",
                "provider": "openai",
                "scope_type": "global",
                "scope_id": None,
                "encrypted_value": "global_key",
                "is_active": True,
            },
            # Account token
            {
                "token_type": "llm",
                "provider": "openai",
                "scope_type": "account",
                "scope_id": "acme",
                "encrypted_value": "acme_key",
                "is_active": True,
            },
            # Project token (most specific)
            {
                "token_type": "llm",
                "provider": "openai",
                "scope_type": "project",
                "scope_id": "backend",
                "encrypted_value": "backend_key",
                "is_active": True,
            },
        ]
        self.configs = []

    def fetchone(self, query, params=None):
        # Mock token query
        if "api_tokens" in query:
            for token in self.tokens:
                if (
                    token["token_type"] == params[0]
                    and token["provider"] == params[1]
                    and token["scope_type"] == params[2]
                ):
                    if len(params) == 3:
                        if token["scope_id"] is None:
                            return token
                    elif len(params) == 4 and token["scope_id"] == params[3]:
                        return token

        # Mock config query
        if "configs" in query and "key_name" in query:
            return None

        return None

    def fetchall(self, query, params=None):
        return []

    def execute(self, query, params=None):
        pass


def test_scope_based_token_resolution():
    """Test that LLMClient uses ScopeResolver for token resolution."""
    db = MockDB()

    # Test 1: With project scope - should use project token
    client = LLMClient(
        db=db, user_id="alice", tenant_id="acme", scope_context={"project": "backend"}
    )

    # Should resolve to project-level token
    key = client._get_api_key("openai")
    assert key == "backend_key"

    # Test 2: Without project scope - should fall back to account token
    client = LLMClient(db=db, user_id="alice", tenant_id="acme")

    key = client._get_api_key("openai")
    assert key == "acme_key"

    # Test 3: Update context dynamically
    client.set_user_context(user_id="alice", tenant_id="acme", scope_context={"project": "backend"})

    key = client._get_api_key("openai")
    assert key == "backend_key"


def test_scope_chain_builder_integration():
    """Test different scope chain builders."""
    MockDB()

    # Dev Agent scenario
    chain = ScopeChainBuilder.dev_agent(
        user_id="alice", account_id="acme", repo="matrixone", project="backend"
    )

    assert len(chain) == 5
    assert chain[0] == ("repo", "matrixone")
    assert chain[1] == ("project", "backend")
    assert chain[2] == ("user", "alice")
    assert chain[3] == ("account", "acme")
    assert chain[4] == ("global", None)

    # Sales Agent scenario
    chain = ScopeChainBuilder.sales_agent(user_id="bob", account_id="sales_corp", region="us-west")

    assert chain[0] == ("region", "us-west")
    assert chain[1] == ("user", "bob")


def test_scope_resolver_with_real_db_structure():
    """Test ScopeResolver with realistic database structure."""
    db = MockDB()

    # Build scope chain
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme", project="backend")

    resolver = ScopeResolver(db, chain)

    # Resolve token - should get project-level token
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["scope_type"] == "project"
    assert token["encrypted_value"] == "backend_key"


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
