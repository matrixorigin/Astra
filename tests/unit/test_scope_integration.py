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
    session.execute(delete(Token))
    session.commit()
    yield session
    session.execute(delete(Token))
    session.commit()
    session.close()


def test_scope_based_token_resolution(db):
    """Test that ScopeResolver resolves tokens correctly."""
    from uuid_utils import uuid7
    from core.auth.encryption import encrypt_token

    # Global token
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value=encrypt_token("global_key"),
        is_active=True,
    ))
    # User token
    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        scope_user_id="alice",
        encrypted_value=encrypt_token("alice_key"),
        is_active=True,
    ))
    db.commit()

    # With user scope — should use user token
    chain = ScopeChainBuilder.dev_agent(user_id="alice")
    resolver = ScopeResolver(lambda: db, chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "alice_key"

    # Without user token — should fall back to global
    chain = ScopeChainBuilder.dev_agent(user_id="bob")
    resolver = ScopeResolver(lambda: db, chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["encrypted_value"] == "global_key"


def test_scope_chain_builder_integration():
    """Test different scope chain builders."""
    chain = ScopeChainBuilder.dev_agent(user_id="alice", repo="matrixone")
    assert len(chain) >= 2
    assert ("user", "alice") in chain
    assert ("global", None) in chain


def test_scope_resolver_with_real_db_structure(db):
    """Test ScopeResolver with realistic database structure."""
    from uuid_utils import uuid7
    from core.auth.encryption import encrypt_token

    db.add(Token(
        token_id=str(uuid7()),
        type="llm",
        provider="openai",
        encrypted_value=encrypt_token("global_key"),
        is_active=True,
    ))
    db.commit()

    chain = ScopeChainBuilder.dev_agent(user_id="alice")
    resolver = ScopeResolver(lambda: db, chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
