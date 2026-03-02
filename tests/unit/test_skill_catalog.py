"""Tests for SkillCatalog — register, conflict detection, publish/unpublish, queries, lifecycle."""

import uuid

import pytest

from api.models import SkillInstallation, SkillRegistry as SkillModel
from core.exceptions import SkillNotFoundError
from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)
from core.skills.catalog import (
    SOURCE_BUILTIN,
    SOURCE_MARKETPLACE,
    SOURCE_USER,
    NameConflictError,
    SkillCatalog,
)


# ── Helpers ───────────────────────────────────────────────────────────────────


class StubInput(SkillInput):
    text: str = ""


class StubOutput(SkillOutput):
    pass


class StubSkill(Skill[StubInput, StubOutput]):
    """Test stub that matches the real Skill interface contract."""

    def __init__(self, name: str, version: str = "1.0.0"):
        self.name = name
        self.version = version
        self.description = f"stub {name}"
        self.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ,
        )
        self.side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    async def execute(self, input: StubInput) -> StubOutput:
        return StubOutput(success=True, result="ok")


def _uid():
    return str(uuid.uuid4())


def _name(prefix="sk"):
    return f"{prefix}_{uuid.uuid4().hex}"


@pytest.fixture
def catalog(db_factory):
    return SkillCatalog(db_factory)


# ── Register ──────────────────────────────────────────────────────────────────


class TestRegister:
    def test_basic_register(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row is not None
        assert row.source == SOURCE_BUILTIN
        assert row.is_active == 1

    def test_side_effect_profile_persisted(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.side_effect_profile is not None
        assert row.side_effect_profile["category"] == "read"

    def test_new_version_deactivates_old(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n, "1.0.0"), source=SOURCE_BUILTIN)
        catalog.register(StubSkill(n, "2.0.0"), source=SOURCE_BUILTIN)
        v1 = db_session.query(SkillModel).filter(SkillModel.skill_id == f"{n}@1.0.0").first()
        v2 = db_session.query(SkillModel).filter(SkillModel.skill_id == f"{n}@2.0.0").first()
        assert v1.is_active == 0
        assert v2.is_active == 1

    def test_re_register_updates_existing(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN, category="old")
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN, category="new")
        rows = db_session.query(SkillModel).filter(SkillModel.skill_name == n).all()
        assert len(rows) == 1
        assert rows[0].category == "new"

    def test_in_memory_cache(self, catalog):
        n = _name()
        s = StubSkill(n)
        catalog.register(s, source=SOURCE_BUILTIN)
        assert catalog.get(n) is s
        assert catalog.get(n, "1.0.0") is s

    def test_draft_not_active(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN, status="draft")
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.is_active == 0
        assert row.status == "draft"

    def test_invalid_status_raises(self, catalog):
        with pytest.raises(ValueError, match="Invalid status"):
            catalog.register(StubSkill(_name()), source=SOURCE_BUILTIN, status="bogus")

    def test_invalid_source_raises(self, catalog):
        with pytest.raises(ValueError, match="Invalid source"):
            catalog.register(StubSkill(_name()), source="alien")


# ── register_from_api ─────────────────────────────────────────────────────────


class TestRegisterFromApi:
    def test_basic(self, catalog, db_session):
        n = _name()
        sid = f"{n}@1.0.0"
        result = catalog.register_from_api(
            skill_id=sid, skill_name=n, version="1.0.0",
            skill_code="def run(): pass", description="api skill",
        )
        assert result["skill_id"] == sid
        assert result["skill_name"] == n
        row = db_session.query(SkillModel).filter(SkillModel.skill_id == sid).first()
        assert row.is_active == 1
        assert row.side_effect_profile == {"category": "read"}

    def test_deactivates_old_versions(self, catalog, db_session):
        n = _name()
        catalog.register_from_api(
            skill_id=f"{n}@1.0.0", skill_name=n, version="1.0.0",
            skill_code="v1",
        )
        catalog.register_from_api(
            skill_id=f"{n}@2.0.0", skill_name=n, version="2.0.0",
            skill_code="v2",
        )
        v1 = db_session.query(SkillModel).filter(SkillModel.skill_id == f"{n}@1.0.0").first()
        v2 = db_session.query(SkillModel).filter(SkillModel.skill_id == f"{n}@2.0.0").first()
        assert v1.is_active == 0
        assert v2.is_active == 1

    def test_updates_existing(self, catalog, db_session):
        n = _name()
        sid = f"{n}@1.0.0"
        catalog.register_from_api(
            skill_id=sid, skill_name=n, version="1.0.0",
            skill_code="v1", description="old",
        )
        catalog.register_from_api(
            skill_id=sid, skill_name=n, version="1.0.0",
            skill_code="v1-updated", description="new",
        )
        rows = db_session.query(SkillModel).filter(SkillModel.skill_name == n).all()
        assert len(rows) == 1
        assert rows[0].description == "new"

    def test_invalid_source_raises(self, catalog):
        with pytest.raises(ValueError, match="Invalid source"):
            catalog.register_from_api(
                skill_id="x@1", skill_name="x", version="1",
                skill_code="x", source="alien",
            )


# ── Conflict detection ────────────────────────────────────────────────────────


class TestConflictDetection:
    def test_user_cannot_shadow_builtin(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        with pytest.raises(NameConflictError, match="builtin"):
            catalog.publish_user_skill(_uid(), n, "1.0.0", "desc")

    def test_user_cannot_shadow_marketplace(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_MARKETPLACE)
        with pytest.raises(NameConflictError, match="marketplace"):
            catalog.publish_user_skill(_uid(), n, "1.0.0", "desc")

    def test_user_cannot_take_other_users_name(self, catalog):
        n = _name()
        catalog.publish_user_skill(_uid(), n, "1.0.0", "desc")
        with pytest.raises(NameConflictError, match="another user"):
            catalog.publish_user_skill(_uid(), n, "1.0.0", "desc")

    def test_same_user_can_update(self, catalog):
        n = _name()
        uid = _uid()
        catalog.publish_user_skill(uid, n, "1.0.0", "v1")
        result = catalog.publish_user_skill(uid, n, "1.1.0", "v2")
        assert result["version"] == "1.1.0"


# ── Publish / Unpublish ──────────────────────────────────────────────────────


class TestPublishUnpublish:
    def test_publish_creates_user_skill(self, catalog, db_session):
        n = _name()
        uid = _uid()
        result = catalog.publish_user_skill(uid, n, "1.0.0", "A skill")
        assert result["status"] == "active"
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.source == SOURCE_USER
        assert row.created_by == uid

    def test_unpublish_deletes_when_no_installs(self, catalog, db_session):
        n = _name()
        uid = _uid()
        catalog.publish_user_skill(uid, n, "1.0.0", "desc")
        assert catalog.unpublish_user_skill(uid, n) == "deleted"
        assert db_session.query(SkillModel).filter(SkillModel.skill_name == n).count() == 0

    def test_unpublish_deprecates_when_installed(self, catalog, db_session):
        n = _name()
        owner = _uid()
        catalog.publish_user_skill(owner, n, "1.0.0", "desc")
        # Use commit() not flush() — the catalog opens its own session via
        # _db(), which in tests returns the same db_session.  commit() is
        # safer: if the fixture ever changes to separate connections, the
        # installation row will still be visible to the catalog's query.
        db_session.add(SkillInstallation(
            installation_id=_uid(), user_id=_uid(), skill_name=n,
            skill_version="1.0.0", status="installed",
        ))
        db_session.commit()
        assert catalog.unpublish_user_skill(owner, n) == "deprecated"
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.status == "deprecated"

    def test_unpublish_nonexistent_raises(self, catalog):
        with pytest.raises(SkillNotFoundError):
            catalog.unpublish_user_skill(_uid(), "no_such_skill")

    def test_version_bump_deactivates_old(self, catalog, db_session):
        n = _name()
        uid = _uid()
        catalog.publish_user_skill(uid, n, "1.0.0", "v1")
        catalog.publish_user_skill(uid, n, "2.0.0", "v2")
        active = db_session.query(SkillModel).filter(
            SkillModel.skill_name == n, SkillModel.is_active == 1,
        ).all()
        assert len(active) == 1
        assert active[0].version == "2.0.0"


# ── Queries ───────────────────────────────────────────────────────────────────


class TestQueries:
    def test_list_by_source(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        names = [r["skill_name"] for r in catalog.list_by_source(SOURCE_BUILTIN)]
        assert n in names

    def test_list_by_owner(self, catalog):
        n = _name()
        uid = _uid()
        catalog.publish_user_skill(uid, n, "1.0.0", "desc")
        assert any(r["skill_name"] == n for r in catalog.list_by_owner(uid))

    def test_list_active(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        result = catalog.list_active(limit=200)
        assert result["total"] >= 1
        assert n in [s["skill_name"] for s in result["skills"]]

    def test_get_skill_info(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        info = catalog.get_skill_info(n)
        assert info["skill_name"] == n
        assert info["install_count"] == 0

    def test_get_visible_skills(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        result = catalog.get_visible_skills(_uid())
        assert n in [s["skill_name"] for s in result[SOURCE_BUILTIN]]

    def test_list_versions(self, catalog):
        n = _name()
        catalog.register(StubSkill(n, "1.0.0"), source=SOURCE_BUILTIN, is_active=False)
        catalog.register(StubSkill(n, "2.0.0"), source=SOURCE_BUILTIN)
        assert len(catalog.list_versions(n)) == 2

    def test_get_metadata(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        meta = catalog.get_metadata(n)
        assert meta["skill_name"] == n
        assert meta["source"] == SOURCE_BUILTIN

    def test_get_not_found_raises(self, catalog):
        with pytest.raises(SkillNotFoundError):
            catalog.get("nonexistent_xyz_999")

    def test_metadata_cache_invalidated_on_mutation(self, catalog):
        n = _name()
        catalog.register(StubSkill(n, "1.0.0"), source=SOURCE_BUILTIN)
        meta1 = catalog.get_metadata(n)
        assert meta1["version"] == "1.0.0"
        catalog.register(StubSkill(n, "2.0.0"), source=SOURCE_BUILTIN)
        meta2 = catalog.get_metadata(n)
        assert meta2["version"] == "2.0.0"


# ── Lifecycle ─────────────────────────────────────────────────────────────────


class TestLifecycle:
    def test_rollback(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n, "1.0.0"), source=SOURCE_BUILTIN)
        catalog.register(StubSkill(n, "2.0.0"), source=SOURCE_BUILTIN)
        assert catalog.rollback(n) == "1.0.0"
        db_session.expire_all()
        assert db_session.query(SkillModel).filter(SkillModel.skill_id == f"{n}@1.0.0").first().is_active == 1
        assert db_session.query(SkillModel).filter(SkillModel.skill_id == f"{n}@2.0.0").first().is_active == 0

    def test_rollback_no_active_raises(self, catalog):
        with pytest.raises(ValueError, match="No active version"):
            catalog.rollback("nonexistent_xyz")

    def test_rollback_no_previous_raises(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        with pytest.raises(ValueError, match="No previous version"):
            catalog.rollback(n)

    def test_uninstall(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n, "1.0.0"), source=SOURCE_BUILTIN, is_active=False)
        catalog.register(StubSkill(n, "2.0.0"), source=SOURCE_BUILTIN)
        assert catalog.uninstall(n) == 2
        assert db_session.query(SkillModel).filter(SkillModel.skill_name == n).count() == 0

    def test_deprecate(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        catalog.deprecate(n)
        db_session.expire_all()
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.status == "deprecated"
        assert row.is_active == 0

    def test_set_status(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        assert catalog.set_status(n, "1.0.0", "deprecated") is True
        db_session.expire_all()
        assert db_session.query(SkillModel).filter(SkillModel.skill_name == n).first().status == "deprecated"

    def test_set_status_invalid_transition(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN, status="draft")
        with pytest.raises(ValueError, match="Invalid transition"):
            catalog.set_status(n, "1.0.0", "archived")

    def test_activate_from_draft(self, catalog, db_session):
        """activate() delegates to set_status() — single code path for transitions."""
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN, status="draft")
        catalog.activate(n, "1.0.0")
        db_session.expire_all()
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.status == "active"
        assert row.is_active == 1

    def test_activate_from_deprecated(self, catalog, db_session):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        catalog.deprecate(n)
        catalog.activate(n)  # no version — picks latest deprecated
        db_session.expire_all()
        row = db_session.query(SkillModel).filter(SkillModel.skill_name == n).first()
        assert row.status == "active"
        assert row.is_active == 1

    def test_activate_nothing_to_activate_raises(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        # Already active — no draft/deprecated version to activate
        with pytest.raises(ValueError, match="No draft/deprecated"):
            catalog.activate(n)


# ── get_metadata_by_id ────────────────────────────────────────────────────────


class TestGetMetadataById:
    def test_found(self, catalog):
        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)
        meta = catalog.get_metadata_by_id(f"{n}@1.0.0")
        assert meta is not None
        assert meta["skill_name"] == n

    def test_not_found(self, catalog):
        assert catalog.get_metadata_by_id("nonexistent@9.9.9") is None


# ── list_available ────────────────────────────────────────────────────────────


class TestListAvailable:
    def test_returns_compatible_skills(self, catalog, db_session):
        from api.models import Repo as RepoModel

        n = _name()
        catalog.register(StubSkill(n), source=SOURCE_BUILTIN)

        # Seed a repo with matching type/access
        rid = _uid()
        db_session.add(RepoModel(
            repo_id=rid,
            user_id="test",
            repo_url="https://example.com/repo",
            repo_name="test-repo",
            repo_type="code",
            access_scope="read",
        ))
        db_session.commit()

        available = catalog.list_available(rid)
        names = [s.name for s in available]
        assert n in names

    def test_excludes_incompatible_access(self, catalog, db_session):
        """A read-only repo must NOT see skills requiring write access."""
        from api.models import Repo as RepoModel

        write_skill = StubSkill(_name())
        write_skill.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.WRITE,
        )
        catalog.register(write_skill, source=SOURCE_BUILTIN)

        rid = _uid()
        db_session.add(RepoModel(
            repo_id=rid,
            user_id="test",
            repo_url="https://example.com/repo2",
            repo_name="read-repo",
            repo_type="code",
            access_scope="read",
        ))
        db_session.commit()

        names = [s.name for s in catalog.list_available(rid)]
        assert write_skill.name not in names

    def test_no_repo_returns_empty(self, catalog):
        assert catalog.list_available("nonexistent-repo-id") == []


# ── Negative caching (_CACHE_MISS) ───────────────────────────────────────────


class TestNegativeCaching:
    def test_nonexistent_skill_cached_as_miss(self, catalog):
        """First lookup of nonexistent skill hits DB; second returns None from cache."""
        result1 = catalog.get_metadata("no_such_skill_xyz")
        assert result1 is None
        # Cache should now contain a _CACHE_MISS entry
        assert len(catalog._metadata_cache) == 1

        # Second lookup — same result, no new cache entry (cache hit)
        result2 = catalog.get_metadata("no_such_skill_xyz")
        assert result2 is None
        assert len(catalog._metadata_cache) == 1

    def test_invalidation_clears_negative_entries(self, catalog):
        """Negative cache entries must be cleared on mutation."""
        catalog.get_metadata("ghost_skill")
        assert len(catalog._metadata_cache) == 1

        # Register a skill — should clear all cache including negative entries
        catalog.register(StubSkill(_name()), source=SOURCE_BUILTIN)
        assert len(catalog._metadata_cache) == 0
