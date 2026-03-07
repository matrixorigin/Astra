"""Integration tests: ModelQuirks stored in infra_llm_models.quirks column.

Verifies:
- CREATE with quirks → all quirk fields persisted correctly in DB
- CREATE without quirks → quirks column is NULL (not hardcoded defaults)
- UPDATE quirks → DB updated, load_from_db reflects new values
- load_from_db reads quirks column (no hardcoded provider logic)
- ModelResponse includes quirks in API response
- strict_tool_call_ids persisted and loaded correctly
- QuirksSchema ↔ ModelQuirks field parity
"""

import json

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text
from uuid import uuid4

from api.database import get_db_session
from api.main import app
from core.llm.router import ModelRegistry, ModelQuirks


@pytest.fixture
def client(db_session):
    def override_get_db():
        try:
            yield db_session
        finally:
            pass
    app.dependency_overrides[get_db_session] = override_get_db
    yield TestClient(app)
    app.dependency_overrides.clear()


@pytest.fixture(autouse=True)
def clean_models(db_session):
    db_session.execute(text("DELETE FROM infra_llm_models WHERE model_name LIKE 'test-%'"))
    db_session.commit()
    yield
    db_session.execute(text("DELETE FROM infra_llm_models WHERE model_name LIKE 'test-%'"))
    db_session.commit()


def _create(client, admin_headers, **overrides):
    payload = {"name": "test-model", "provider": "mock", "api_key": "sk-test", **overrides}
    return client.post("/models", headers=admin_headers, json=payload)


def _all_quirk_fields() -> set[str]:
    """Return all field names defined in ModelQuirks."""
    return set(ModelQuirks.model_fields.keys())


# ── Schema Parity ───────────────────────────────────────────────────────────

class TestQuirksSchemaParity:
    """QuirksSchema (API) must have the same fields as ModelQuirks (core)."""

    def test_quirks_schema_has_all_model_quirks_fields(self):
        """Every field in ModelQuirks must exist in QuirksSchema."""
        from api.routers.models import QuirksSchema
        core_fields = _all_quirk_fields()
        api_fields = set(QuirksSchema.model_fields.keys())
        missing = core_fields - api_fields
        assert not missing, (
            f"QuirksSchema is missing fields from ModelQuirks: {missing}. "
            f"API cannot set these quirks via REST."
        )


# ── CREATE ──────────────────────────────────────────────────────────────────

class TestCreateModelQuirks:
    def test_create_with_full_quirks_persists_all_fields(self, client, admin_headers, db_session):
        """Every quirk field must be persisted exactly as sent."""
        quirks = {
            "fixed_temperature": 1.0,
            "preserve_reasoning_content": True,
            "no_parallel_tool_calls": True,
            "tool_choice_required": True,
            "strict_tool_call_ids": True,
            "no_system_message": False,
            "system_as_user_prefix": False,
        }
        resp = _create(client, admin_headers, name="test-kimi", provider="moonshot", quirks=quirks)
        assert resp.status_code == 201

        # Ground truth: re-query DB directly
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-kimi'")
        ).fetchone()
        assert row is not None
        assert row[0] is not None, "quirks column must not be NULL"

        stored = row[0] if isinstance(row[0], dict) else json.loads(row[0])
        assert stored["fixed_temperature"] == 1.0
        assert stored["preserve_reasoning_content"] is True
        assert stored["no_parallel_tool_calls"] is True
        assert stored["tool_choice_required"] is True
        assert stored["strict_tool_call_ids"] is True
        assert stored["no_system_message"] is False
        assert stored["system_as_user_prefix"] is False

    def test_create_without_quirks_all_defaults(self, client, admin_headers, db_session):
        """Models without quirks: every field must be at its default value."""
        resp = _create(client, admin_headers, name="test-plain", provider="openai")
        assert resp.status_code == 201

        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-plain'")
        ).fetchone()
        assert row is not None
        stored = row[0]
        if stored is not None:
            d = stored if isinstance(stored, dict) else json.loads(stored)
            # Verify ALL fields are at default — not just a spot check
            defaults = ModelQuirks()
            for field_name in _all_quirk_fields():
                expected = getattr(defaults, field_name)
                actual = d.get(field_name)
                # None and missing are both acceptable as "default"
                if expected is None:
                    assert actual is None or field_name not in d, (
                        f"Field {field_name}: expected None/absent, got {actual}"
                    )
                elif expected is False:
                    assert actual in (False, None) or field_name not in d, (
                        f"Field {field_name}: expected False/absent, got {actual}"
                    )

    def test_create_quirks_reflected_in_api_response(self, client, admin_headers):
        """API response must include quirks with correct values."""
        quirks = {"fixed_temperature": 1.0, "preserve_reasoning_content": True}
        resp = _create(client, admin_headers, name="test-resp", provider="moonshot", quirks=quirks)
        assert resp.status_code == 201
        data = resp.json()
        assert "quirks" in data
        assert data["quirks"]["fixed_temperature"] == 1.0
        assert data["quirks"]["preserve_reasoning_content"] is True

    def test_create_non_moonshot_has_no_quirks_injected(self, client, admin_headers):
        """Non-moonshot providers must NOT get quirks injected by provider name."""
        resp = _create(client, admin_headers, name="test-gpt", provider="openai")
        assert resp.status_code == 201
        data = resp.json()
        # Verify ALL quirk fields are at defaults
        defaults = ModelQuirks()
        for field_name in _all_quirk_fields():
            expected = getattr(defaults, field_name)
            actual = data["quirks"][field_name]
            assert actual == expected, (
                f"Field {field_name}: expected default {expected}, got {actual}"
            )


# ── UPDATE ──────────────────────────────────────────────────────────────────

class TestUpdateModelQuirks:
    def test_update_quirks_persists_to_db(self, client, admin_headers, db_session):
        """PUT /models/{name} with quirks must update DB column."""
        _create(client, admin_headers, name="test-upd", provider="mock")

        resp = client.put(
            "/models/test-upd",
            headers=admin_headers,
            json={"quirks": {"fixed_temperature": 0.7, "no_parallel_tool_calls": True}},
        )
        assert resp.status_code == 200

        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-upd'")
        ).fetchone()
        assert row is not None
        stored = row[0] if isinstance(row[0], dict) else json.loads(row[0])
        assert stored["fixed_temperature"] == 0.7
        assert stored["no_parallel_tool_calls"] is True

    def test_update_quirks_reflected_in_response(self, client, admin_headers):
        _create(client, admin_headers, name="test-upd2", provider="mock")
        resp = client.put(
            "/models/test-upd2",
            headers=admin_headers,
            json={"quirks": {"fixed_temperature": 1.0}},
        )
        assert resp.status_code == 200
        assert resp.json()["quirks"]["fixed_temperature"] == 1.0

    def test_update_quirks_does_not_affect_other_columns(self, client, admin_headers, db_session):
        """Updating quirks must not change other model fields."""
        _create(client, admin_headers, name="test-upd3", provider="mock",
                tags=["code"], architecture="transformer")

        # Read before
        before = db_session.execute(
            text("SELECT tags, architecture, provider FROM infra_llm_models WHERE model_name = 'test-upd3'")
        ).fetchone()

        client.put(
            "/models/test-upd3",
            headers=admin_headers,
            json={"quirks": {"fixed_temperature": 0.5}},
        )

        # Read after
        after = db_session.execute(
            text("SELECT tags, architecture, provider FROM infra_llm_models WHERE model_name = 'test-upd3'")
        ).fetchone()

        assert before[0] == after[0]  # tags unchanged
        assert before[1] == after[1]  # architecture unchanged
        assert before[2] == after[2]  # provider unchanged


# ── load_from_db ─────────────────────────────────────────────────────────────

class TestLoadFromDb:
    def _insert_model(self, db_session, model_name: str, provider: str, quirks: dict | None = None):
        """Insert directly into DB (bypasses connectivity check, sets is_active=1)."""
        db_session.execute(
            text(
                "INSERT INTO infra_llm_models "
                "(model_id, model_name, provider, api_key_encrypted, is_active, quirks) "
                "VALUES (:id, :name, :prov, 'enc-key', 1, :quirks)"
            ),
            {
                "id": str(uuid4()),
                "name": model_name,
                "prov": provider,
                "quirks": json.dumps(quirks) if quirks is not None else None,
            },
        )
        db_session.commit()

    def test_load_from_db_reads_quirks_column(self, db_session):
        """ModelRegistry.load_from_db must read quirks from DB, not hardcode by provider."""
        self._insert_model(db_session, "test-load", "moonshot",
                           quirks={"fixed_temperature": 1.0, "preserve_reasoning_content": True,
                                   "strict_tool_call_ids": True})

        registry = ModelRegistry()
        registry.load_from_db(db_session)

        cfg = registry.get("test-load")
        assert cfg is not None
        assert cfg.quirks.fixed_temperature == 1.0
        assert cfg.quirks.preserve_reasoning_content is True
        assert cfg.quirks.strict_tool_call_ids is True
        assert cfg.quirks.no_parallel_tool_calls is False  # default

    def test_load_from_db_no_quirks_gives_all_defaults(self, db_session):
        """Model without quirks must load with ALL ModelQuirks fields at default."""
        self._insert_model(db_session, "test-noq", "openai", quirks=None)

        registry = ModelRegistry()
        registry.load_from_db(db_session)

        cfg = registry.get("test-noq")
        assert cfg is not None
        defaults = ModelQuirks()
        for field_name in _all_quirk_fields():
            actual = getattr(cfg.quirks, field_name)
            expected = getattr(defaults, field_name)
            assert actual == expected, (
                f"Field {field_name}: expected default {expected}, got {actual}"
            )

    def test_load_from_db_no_hardcoded_provider_logic(self, db_session):
        """A moonshot model WITHOUT quirks in DB must NOT get quirks injected."""
        self._insert_model(db_session, "test-moon-plain", "moonshot", quirks=None)

        registry = ModelRegistry()
        registry.load_from_db(db_session)

        cfg = registry.get("test-moon-plain")
        assert cfg is not None
        # Must NOT have fixed_temperature=1.0 injected by provider name
        assert cfg.quirks.fixed_temperature is None
        assert cfg.quirks.preserve_reasoning_content is False
        assert cfg.quirks.strict_tool_call_ids is False

    def test_load_from_db_strict_tool_call_ids(self, db_session):
        """strict_tool_call_ids must be read from DB quirks column."""
        self._insert_model(db_session, "test-strict", "moonshot",
                           quirks={"strict_tool_call_ids": True})

        registry = ModelRegistry()
        registry.load_from_db(db_session)

        cfg = registry.get("test-strict")
        assert cfg is not None
        assert cfg.quirks.strict_tool_call_ids is True
        # Other quirks should be at defaults
        assert cfg.quirks.fixed_temperature is None
        assert cfg.quirks.preserve_reasoning_content is False

    def test_load_from_db_quirks_as_json_string(self, db_session):
        """quirks stored as JSON string (not native JSON) must still parse correctly."""
        # Some DB drivers return JSON columns as strings
        db_session.execute(
            text(
                "INSERT INTO infra_llm_models "
                "(model_id, model_name, provider, api_key_encrypted, is_active, quirks) "
                "VALUES (:id, :name, :prov, 'enc-key', 1, :quirks)"
            ),
            {
                "id": str(uuid4()),
                "name": "test-jsonstr",
                "prov": "mock",
                "quirks": '{"fixed_temperature": 0.5, "no_system_message": true}',
            },
        )
        db_session.commit()

        registry = ModelRegistry()
        registry.load_from_db(db_session)

        cfg = registry.get("test-jsonstr")
        assert cfg is not None
        assert cfg.quirks.fixed_temperature == 0.5
        assert cfg.quirks.no_system_message is True


# ── ModelQuirks unit tests ──────────────────────────────────────────────────

class TestModelQuirksDefaults:
    """Verify ModelQuirks default values are safe (no accidental True/non-None)."""

    def test_all_bool_fields_default_false(self):
        """All boolean quirk fields must default to False (opt-in, not opt-out)."""
        q = ModelQuirks()
        for name, field_info in ModelQuirks.model_fields.items():
            if field_info.annotation is bool:
                assert getattr(q, name) is False, (
                    f"Boolean quirk {name} defaults to True — must be False (opt-in)"
                )

    def test_all_optional_fields_default_none(self):
        """All Optional fields must default to None."""
        q = ModelQuirks()
        for name, field_info in ModelQuirks.model_fields.items():
            annotation = field_info.annotation
            # Check for float | None pattern
            if hasattr(annotation, "__args__") and type(None) in getattr(annotation, "__args__", ()):
                assert getattr(q, name) is None, (
                    f"Optional quirk {name} has non-None default"
                )


# ── Startup migration regression ────────────────────────────────────────────

class TestInitDbSyncsSeedQuirks:
    """Regression: init_db must sync quirks from SEED_MODELS on every startup.

    Root cause (2026-03-07): kimi-k2.5 had empty quirks in DB because the
    one-time backfill ran before fixed_temperature was added to the seed config.
    Result: temperature=0.7 sent to kimi-k2.5 → API 400 "only 1 is allowed".
    """

    SEED_MODEL_NAME = "test-seed-quirks-sync"

    @pytest.fixture(autouse=True)
    def _seed_row(self, client, admin_headers, db_session):
        """Create a model row that mimics a seed model with quirks."""
        from core.llm.seed_models import SEED_MODELS
        seeds = [s for s in SEED_MODELS if s.get("quirks")]
        if not seeds:
            pytest.skip("No seed models with quirks defined")
        self._seed_quirks = seeds[0]["quirks"]

        # Create via API so all required columns are populated
        _create(client, admin_headers,
                name=self.SEED_MODEL_NAME, provider="moonshot",
                quirks=self._seed_quirks)
        yield
        db_session.execute(
            text("DELETE FROM infra_llm_models WHERE model_name = :m"),
            {"m": self.SEED_MODEL_NAME},
        )
        db_session.commit()

    def test_empty_quirks_overwritten_by_seed(self, db_session):
        """If a seed model has quirks={} in DB, init_db must restore seed values."""
        from core.llm.seed_models import SEED_MODELS
        import api.database as db_mod

        # Temporarily patch SEED_MODELS so init_db syncs our test model
        patched = [{"model_name": self.SEED_MODEL_NAME, "quirks": self._seed_quirks}]
        original = SEED_MODELS.copy()

        # Corrupt: set quirks to empty
        db_session.execute(
            text("UPDATE infra_llm_models SET quirks = '{}' WHERE model_name = :m"),
            {"m": self.SEED_MODEL_NAME},
        )
        db_session.commit()

        # Verify corruption
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = :m"),
            {"m": self.SEED_MODEL_NAME},
        ).fetchone()
        assert row[0] == "{}" or row[0] == {}

        # Monkey-patch seed models and re-run init_db
        import core.llm.seed_models as sm_mod
        old = sm_mod.SEED_MODELS
        try:
            sm_mod.SEED_MODELS = patched
            db_mod.init_db()
        finally:
            sm_mod.SEED_MODELS = old

        # Verify quirks restored
        db_session.expire_all()
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = :m"),
            {"m": self.SEED_MODEL_NAME},
        ).fetchone()
        quirks = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        for key, val in self._seed_quirks.items():
            assert quirks.get(key) == val, (
                f"quirks.{key} expected {val}, got {quirks.get(key)}"
            )

    def test_fixed_temperature_survives_restart(self, db_session):
        """fixed_temperature must survive init_db restart — the exact bug."""
        import core.llm.seed_models as sm_mod
        import api.database as db_mod

        patched = [{"model_name": self.SEED_MODEL_NAME, "quirks": self._seed_quirks}]
        old = sm_mod.SEED_MODELS
        try:
            sm_mod.SEED_MODELS = patched
            db_mod.init_db()
        finally:
            sm_mod.SEED_MODELS = old

        db_session.expire_all()
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = :m"),
            {"m": self.SEED_MODEL_NAME},
        ).fetchone()
        quirks = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        assert quirks.get("fixed_temperature") == self._seed_quirks["fixed_temperature"]

    def test_admin_customized_quirks_not_overwritten(self, db_session):
        """init_db must NOT overwrite quirks that an admin has customized.

        Only NULL or '{}' quirks are backfilled from seed — non-empty quirks
        are treated as admin-owned and preserved across restarts.
        """
        import core.llm.seed_models as sm_mod
        import api.database as db_mod

        # Admin customizes: sets fixed_temperature to 0.8 for testing
        admin_quirks = {"fixed_temperature": 0.8, "strict_tool_call_ids": False}
        db_session.execute(
            text("UPDATE infra_llm_models SET quirks = :q WHERE model_name = :m"),
            {"q": json.dumps(admin_quirks), "m": self.SEED_MODEL_NAME},
        )
        db_session.commit()

        patched = [{"model_name": self.SEED_MODEL_NAME, "quirks": self._seed_quirks}]
        old = sm_mod.SEED_MODELS
        try:
            sm_mod.SEED_MODELS = patched
            db_mod.init_db()
        finally:
            sm_mod.SEED_MODELS = old

        db_session.expire_all()
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = :m"),
            {"m": self.SEED_MODEL_NAME},
        ).fetchone()
        quirks = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        # Admin's value must be preserved, NOT overwritten by seed
        assert quirks.get("fixed_temperature") == 0.8
        assert quirks.get("strict_tool_call_ids") is False
