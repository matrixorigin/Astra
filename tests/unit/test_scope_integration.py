"""Integration test for scope-based configuration."""

import pytest
from sqlalchemy import delete

from core.llm.client import LLMClient
from core.scope.scope_resolver import ScopeChainBuilder, ScopeResolver
from api.database import get_db_session
from api.models import Token


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    # Clean up before test
    session.execute(delete(Token))
    session.commit()
    yield session
    # Clean up after test
    session.execute(delete(Token))
    session.commit()
    session.close()


def test_scope_based_token_resolution(db):
    """Test that ScopeResolver resolves tokens correctly."""
    # Create test tokens
    from api.models import Token
    from uuid_utils import uuid7
    
    # Global token
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value="global_key",
        is_active=True,
    ))
    
    # Account token
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        scope_tenant_id="acme",
        encrypted_value="acme_key",
        is_active=True,
    ))
    
    db.commit()

    # Test 1: Resolve with account scope - should use account token
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")
    resolver = ScopeResolver(db, chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "acme_key"

    # Test 2: Resolve without account scope - should fall back to global token
    chain = ScopeChainBuilder.dev_agent(user_id="alice")
    resolver = ScopeResolver(db, chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "global_key"


def test_scope_chain_builder_integration():
    """Test different scope chain builders."""
    # Dev Agent scenario
    chain = ScopeChainBuilder.dev_agent(
        user_id="alice", account_id="acme", repo="matrixone"
    )

    assert len(chain) >= 3
    assert ("user", "alice") in chain
    assert ("account", "acme") in chain
    assert ("global", None) in chain


def test_scope_resolver_with_real_db_structure(db):
    """Test ScopeResolver with realistic database structure."""
    from api.models import Token
    from uuid_utils import uuid7
    
    # Create test token
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value="global_key",
        is_active=True,
    ))
    db.commit()

    # Build scope chain
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")

    resolver = ScopeResolver(db, chain)

    # Resolve token - should get global token
    token = resolver.resolve_token("llm", "openai")
    assert token is not None


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
