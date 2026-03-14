"""Tests for repository registry."""

import pytest

from core.repos import AccessScope, OwnerType, RepoRegistry, RepoType


@pytest.fixture
def registry(db_session):
    """Repository registry fixture."""
    return RepoRegistry(lambda: db_session)


@pytest.fixture(autouse=True)
def cleanup(db_session):
    """Clean up test data."""
    yield
    # Only clean after test
    from api.models import Repo

    try:
        db_session.query(Repo).filter(Repo.repo_url.like("%github.com%")).delete(
            synchronize_session=False
        )
        db_session.commit()
    except:
        pass


def test_create_repo(registry, db_session):
    """Test creating a repository."""
    # Clean up first
    from api.models import Repo

    db_session.query(Repo).filter(
        Repo.repo_url == "https://github.com/matrixorigin/matrixone"
    ).delete()
    db_session.commit()

    repo = registry.create(
        repo_url="https://github.com/matrixorigin/matrixone",
        repo_type=RepoType.CODE,
        owner_id="user_123",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
        metadata={"default_branch": "main", "language": "go"},
    )

    assert repo.repo_id
    assert repo.repo_url == "https://github.com/matrixorigin/matrixone"
    assert repo.repo_type == RepoType.CODE
    assert repo.owner_id == "user_123"
    assert repo.access_scope == AccessScope.WRITE
    assert repo.metadata["default_branch"] == "main"
    assert repo.is_active is True

    # Cleanup using ORM
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id == repo.repo_id).delete()
    db_session.commit()


def test_get_repo(registry, db_session):
    """Test getting a repository by ID."""
    repo = registry.create(
        repo_url="https://github.com/matrixorigin/mo-tester",
        repo_type=RepoType.TESTER,
        owner_id="user_123",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.READ,
    )

    retrieved = registry.get(repo.repo_id)
    assert retrieved is not None
    assert retrieved.repo_id == repo.repo_id
    assert retrieved.repo_url == repo.repo_url

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id == repo.repo_id).delete()
    db_session.commit()


def test_get_by_url(registry, db_session):
    """Test getting a repository by URL and owner."""
    repo = registry.create(
        repo_url="https://github.com/matrixorigin/mo-ci",
        repo_type=RepoType.CI,
        owner_id="user_123",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.ADMIN,
    )

    retrieved = registry.get_by_url("https://github.com/matrixorigin/mo-ci", "user_123")
    assert retrieved is not None
    assert retrieved.repo_id == repo.repo_id

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id == repo.repo_id).delete()
    db_session.commit()


def test_list_by_owner(registry, db_session):
    """Test listing repositories by owner."""
    repo1 = registry.create(
        repo_url="https://github.com/user/repo1",
        repo_type=RepoType.CODE,
        owner_id="user_456",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
    )
    repo2 = registry.create(
        repo_url="https://github.com/user/repo2",
        repo_type=RepoType.DOCS,
        owner_id="user_456",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.READ,
    )

    repos = registry.list_by_owner("user_456")
    assert len(repos) == 2
    assert {r.repo_url for r in repos} == {
        "https://github.com/user/repo1",
        "https://github.com/user/repo2",
    }

    # Filter by type
    code_repos = registry.list_by_owner("user_456", repo_type=RepoType.CODE)
    assert len(code_repos) == 1
    assert code_repos[0].repo_type == RepoType.CODE

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id.in_([repo1.repo_id, repo2.repo_id])).delete(
        synchronize_session=False
    )
    db_session.commit()


def test_update_token(registry, db_session):
    """Test updating repository token."""
    repo = registry.create(
        repo_url="https://github.com/test/repo",
        repo_type=RepoType.CODE,
        owner_id="user_789",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
    )

    registry.update_token(repo.repo_id, "token_123")

    updated = registry.get(repo.repo_id)
    assert updated is not None
    assert updated.token_id == "token_123"

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id == repo.repo_id).delete()
    db_session.commit()


def test_update_metadata(registry, db_session):
    """Test updating repository metadata."""
    repo = registry.create(
        repo_url="https://github.com/test/repo2",
        repo_type=RepoType.CODE,
        owner_id="user_789",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
        metadata={"branch": "main"},
    )

    new_metadata = {"branch": "develop", "ci_enabled": True}
    registry.update_metadata(repo.repo_id, new_metadata)

    updated = registry.get(repo.repo_id)
    assert updated is not None
    assert updated.metadata["branch"] == "develop"
    assert updated.metadata["ci_enabled"] is True

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id == repo.repo_id).delete()
    db_session.commit()


def test_deactivate_repo(registry, db_session):
    """Test deactivating a repository."""
    repo = registry.create(
        repo_url="https://github.com/test/repo3",
        repo_type=RepoType.CODE,
        owner_id="user_789",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
    )

    registry.deactivate(repo.repo_id)

    updated = registry.get(repo.repo_id)
    assert updated is not None
    assert updated.is_active is False

    # Should not appear in list
    repos = registry.list_by_owner("user_789")
    assert len(repos) == 0

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id == repo.repo_id).delete()
    db_session.commit()


def test_repo_groups(registry, db_session):
    """Test repository groups functionality."""
    # Create repos with different types
    repo1 = registry.create(
        repo_url="https://github.com/user/group1",
        repo_type=RepoType.CODE,
        owner_id="user_group",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.WRITE,
    )
    repo2 = registry.create(
        repo_url="https://github.com/user/group2",
        repo_type=RepoType.DOCS,
        owner_id="user_group",
        owner_type=OwnerType.USER,
        access_scope=AccessScope.READ,
    )

    # Test filtering
    code_repos = registry.list_by_owner("user_group", repo_type=RepoType.CODE)
    assert len(code_repos) == 1
    assert code_repos[0].repo_url == "https://github.com/user/group1"

    docs_repos = registry.list_by_owner("user_group", repo_type=RepoType.DOCS)
    assert len(docs_repos) == 1
    assert docs_repos[0].repo_url == "https://github.com/user/group2"

    # Cleanup
    from api.models import Repo

    db_session.query(Repo).filter(Repo.repo_id.in_([repo1.repo_id, repo2.repo_id])).delete(
        synchronize_session=False
    )
    db_session.commit()
