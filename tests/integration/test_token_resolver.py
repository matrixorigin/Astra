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
from sdk import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def resolver(db):
    """Token resolver fixture."""
    return TokenResolver(db=db)


@pytest.fixture
def registry(db):
    """Repo registry fixture."""
    return RepoRegistry(db=db)


def test_create_token(resolver, db):
    """Test creating a token."""
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/token1",
        scope_user_id="user_123",
    )

    assert token.token_id
    assert token.token_type == TokenType.REPO
    assert token.provider == "github"
    assert token.scope_user_id == "user_123"
    assert token.is_active is True

    # Cleanup
    db.execute("DELETE FROM tokens WHERE token_id = %s", (token.token_id,))


def test_resolve_user_default_token(resolver, db):
    """Test resolving user default token."""
    # Create user default token
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/user_token",
        scope_user_id="user_456",
    )

    # Resolve
    resolved = resolver.resolve_repo_token(user_id="user_456")
    assert resolved is not None
    assert resolved.token_id == token.token_id
    assert resolved.scope_user_id == "user_456"

    # Cleanup
    db.execute("DELETE FROM tokens WHERE token_id = %s", (token.token_id,))


def test_resolve_tenant_default_token(resolver, db):
    """Test resolving tenant default token."""
    # Create tenant default token
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/tenant_token",
        scope_tenant_id="tenant_789",
    )

    # Resolve (no user token, fallback to tenant)
    resolved = resolver.resolve_repo_token(user_id="user_without_token", tenant_id="tenant_789")
    assert resolved is not None
    assert resolved.token_id == token.token_id
    assert resolved.scope_tenant_id == "tenant_789"

    # Cleanup
    db.execute("DELETE FROM tokens WHERE token_id = %s", (token.token_id,))


def test_resolve_repo_specific_token(resolver, registry, db):
    """Test resolving repo-specific token."""
    # Create repo-specific token
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/repo_token",
        scope_user_id="user_abc",
    )

    # Create repo with this token
    repo = registry.create(
        repo_url="https://github.com/test/repo",
        repo_type=RepoType.CODE,
        owner_id="user_abc",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
        token_id=token.token_id,
    )

    # Resolve by repo_id
    resolved = resolver.resolve_repo_token(user_id="user_abc", repo_id=repo.repo_id)
    assert resolved is not None
    assert resolved.token_id == token.token_id

    # Resolve by repo_url
    resolved = resolver.resolve_repo_token(
        user_id="user_abc", repo_url="https://github.com/test/repo"
    )
    assert resolved is not None
    assert resolved.token_id == token.token_id

    # Cleanup
    registry.delete(repo.repo_id)
    db.execute("DELETE FROM tokens WHERE token_id = %s", (token.token_id,))


def test_token_priority_fallback(resolver, registry, db):
    """Test token resolution priority."""
    # Create tokens at different scopes
    global_token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/global",
    )
    tenant_token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/tenant",
        scope_tenant_id="tenant_xyz",
    )
    user_token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/user",
        scope_user_id="user_xyz",
    )

    # User token should win
    resolved = resolver.resolve_repo_token(user_id="user_xyz", tenant_id="tenant_xyz")
    assert resolved is not None
    assert resolved.token_id == user_token.token_id

    # Deactivate user token, should fallback to tenant
    resolver.deactivate_token(user_token.token_id)
    resolved = resolver.resolve_repo_token(user_id="user_xyz", tenant_id="tenant_xyz")
    assert resolved is not None
    assert resolved.token_id == tenant_token.token_id

    # Cleanup
    db.execute(
        "DELETE FROM tokens WHERE token_id IN (%s, %s, %s)",
        (global_token.token_id, tenant_token.token_id, user_token.token_id),
    )


def test_deactivate_token(resolver, db):
    """Test deactivating a token."""
    token = resolver.create_token(
        token_type=TokenType.REPO,
        provider="github",
        secret_ref="vault://github/test",
        scope_user_id="user_test",
    )

    # Deactivate
    resolver.deactivate_token(token.token_id)

    # Should not be resolved
    resolved = resolver.resolve_repo_token(user_id="user_test")
    assert resolved is None

    # Cleanup
    db.execute("DELETE FROM tokens WHERE token_id = %s", (token.token_id,))
