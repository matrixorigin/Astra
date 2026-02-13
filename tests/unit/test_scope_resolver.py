"""Test scope-based configuration resolver."""

import pytest
from sqlalchemy import delete

from core.scope.scope_resolver import ScopeChainBuilder, ScopeResolver
from api.database import get_db_session
from api.models import Token, Config


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    # Clean up before test
    session.execute(delete(Token))
    session.execute(delete(Config))
    session.commit()
    yield session
    # Clean up after test
    session.execute(delete(Token))
    session.execute(delete(Config))
    session.commit()
    session.close()


def test_resolve_token_priority(db):
    """Test token resolution follows scope priority."""
    from uuid_utils import uuid7
    
    # Add test tokens
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value="global_key",
        is_active=True,
    ))
    
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        scope_tenant_id="acme",
        encrypted_value="acme_key",
        is_active=True,
    ))
    
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        scope_user_id="alice",
        encrypted_value="alice_key",
        is_active=True,
    ))
    
    db.commit()

    # Build chain with user scope
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")
    resolver = ScopeResolver(db, chain)

    # Should resolve to user-level token (most specific in this chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "alice_key"


def test_resolve_token_fallback(db):
    """Test token resolution falls back to less specific scopes."""
    from uuid_utils import uuid7
    
    # Add only global and account tokens
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value="global_key",
        is_active=True,
    ))
    
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        scope_tenant_id="acme",
        encrypted_value="acme_key",
        is_active=True,
    ))
    
    db.commit()

    # Build chain with account scope
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")
    resolver = ScopeResolver(db, chain)

    # Should resolve to account-level token
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "acme_key"


def test_scope_chain_builders():
    """Test different scope chain builders."""
    # Dev agent
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme", repo="matrixone")
    assert ("user", "alice") in chain
    assert ("account", "acme") in chain
    assert ("global", None) in chain

    # Sales agent
    chain = ScopeChainBuilder.sales_agent(user_id="bob", account_id="sales_corp", region="us-west")
    assert ("user", "bob") in chain
    assert ("account", "sales_corp") in chain
    assert ("global", None) in chain

    # Deploy agent
    chain = ScopeChainBuilder.deploy_agent(account_id="devops", environment="prod")
    assert ("environment", "prod") in chain
    assert ("account", "devops") in chain
    assert ("global", None) in chain


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
