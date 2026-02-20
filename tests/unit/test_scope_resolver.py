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
    session.execute(delete(Token))
    session.execute(delete(Config))
    session.commit()
    yield session
    session.execute(delete(Token))
    session.execute(delete(Config))
    session.commit()
    session.close()


def test_resolve_token_priority(db):
    """Test token resolution follows scope priority: user > global."""
    from uuid_utils import uuid7

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
        scope_user_id="alice",
        encrypted_value="alice_key",
        is_active=True,
    ))
    db.commit()

    chain = ScopeChainBuilder.dev_agent(user_id="alice")
    resolver = ScopeResolver(db, chain)

    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "alice_key"


def test_resolve_token_fallback(db):
    """Test token resolution falls back to global when no user token."""
    from uuid_utils import uuid7

    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value="global_key",
        is_active=True,
    ))
    db.commit()

    chain = ScopeChainBuilder.dev_agent(user_id="alice")
    resolver = ScopeResolver(db, chain)

    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "global_key"


def test_scope_chain_builders():
    """Test different scope chain builders."""
    chain = ScopeChainBuilder.dev_agent(user_id="alice", repo="matrixone")
    assert ("user", "alice") in chain
    assert ("repo", "matrixone") in chain
    assert ("global", None) in chain

    chain = ScopeChainBuilder.sales_agent(user_id="bob", region="us-west")
    assert ("user", "bob") in chain
    assert ("region", "us-west") in chain
    assert ("global", None) in chain

    chain = ScopeChainBuilder.deploy_agent(environment="prod")
    assert ("environment", "prod") in chain
    assert ("global", None) in chain
