"""Tests for token resolution."""

import pytest

from core.repos import (
    AccessScope,
    OwnerType,
    RepoRegistry,
    RepoType,
    TokenResolver,
    TokenType,
)


@pytest.fixture
def resolver(db_session):
    """Token resolver fixture."""
    return TokenResolver(lambda: db_session)


@pytest.fixture
def registry(db_session):
    """Repo registry fixture."""
    return RepoRegistry(lambda: db_session)


@pytest.fixture(autouse=True)
def cleanup(db_session):
    """Clean up test data."""
    from api.models import Repo, Token
    db_session.query(Repo).filter(Repo.repo_url.like('%test/repo%')).delete(synchronize_session=False)
    db_session.query(Token).filter(Token.provider == 'github').delete(synchronize_session=False)
    db_session.commit()
    yield
    db_session.query(Repo).filter(Repo.repo_url.like('%test/repo%')).delete(synchronize_session=False)
    db_session.query(Token).filter(Token.provider == 'github').delete(synchronize_session=False)
    db_session.commit()


def test_create_token(resolver, db_session):
    """Test creating a token."""
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/token1",
        scope_user_id="user_123",
    )
    assert token.token_id
    assert token.token_type == TokenType.REPO
    assert token.scope_user_id == "user_123"
    assert token.is_active is True

    from api.models import Token; db_session.query(Token).filter(Token.token_id == token.token_id).delete(); db_session.commit()


def test_resolve_user_default_token(resolver, db_session):
    """Test resolving user default token."""
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/user_token",
        scope_user_id="user_456",
    )
    resolved = resolver.resolve_repo_token(user_id="user_456")
    assert resolved is not None
    assert resolved.token_id == token.token_id

    from api.models import Token; db_session.query(Token).filter(Token.token_id == token.token_id).delete(); db_session.commit()


def test_resolve_repo_specific_token(resolver, registry, db_session):
    """Test resolving repo-specific token."""
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/repo_token",
        scope_user_id="user_abc",
    )
    repo = registry.create(
        repo_url="https://github.com/test/repo",
        repo_type=RepoType.CODE,
        owner_id="user_abc",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
        token_id=token.token_id,
    )

    resolved = resolver.resolve_repo_token(user_id="user_abc", repo_id=repo.repo_id)
    assert resolved is not None
    assert resolved.token_id == token.token_id

    resolved = resolver.resolve_repo_token(
        user_id="user_abc", repo_url="https://github.com/test/repo"
    )
    assert resolved is not None
    assert resolved.token_id == token.token_id

    registry.delete(repo.repo_id)
    from api.models import Token; db_session.query(Token).filter(Token.token_id == token.token_id).delete(); db_session.commit()


def test_token_priority_fallback(resolver, db_session):
    """Test token resolution priority: user > global."""
    user_token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/user",
        scope_user_id="user_xyz",
    )

    # User token should win
    resolved = resolver.resolve_repo_token(user_id="user_xyz")
    assert resolved is not None
    assert resolved.token_id == user_token.token_id

    # Deactivate user token, should return None (no global token)
    resolver.deactivate_token(user_token.token_id)
    resolved = resolver.resolve_repo_token(user_id="user_xyz")
    assert resolved is None

    from api.models import Token
    db_session.query(Token).filter(Token.token_id == user_token.token_id).delete(synchronize_session=False)
    db_session.commit()


def test_deactivate_token(resolver, db_session):
    """Test deactivating a token."""
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/test",
        scope_user_id="user_test",
    )
    resolver.deactivate_token(token.token_id)
    resolved = resolver.resolve_repo_token(user_id="user_test")
    assert resolved is None

    from api.models import Token; db_session.query(Token).filter(Token.token_id == token.token_id).delete(); db_session.commit()
