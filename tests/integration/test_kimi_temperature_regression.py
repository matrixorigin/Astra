"""Regression tests: kimi-k2.5 temperature=1.0 constraint must survive registration.

Root cause (2026-03-07): mo-admin model add kimi-k2.5 didn't pass quirks,
so DB got empty quirks → temperature=0.7 sent to API → 400 "only 1 is allowed".

Covers:
- _get_seed_defaults returns correct quirks for known models
- POST /models for kimi-k2.5 without explicit quirks auto-fills from seed
- POST /models for unknown model gets empty quirks (no injection)
- init_db seed sync restores quirks if they were wiped
- ModelRegistry.load_from_db picks up fixed_temperature from DB
"""

import json

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text
from uuid import uuid4

from api.database import get_db_session
from api.main import app
from core.llm.router import ModelRegistry
from core.llm.seed_models import SEED_MODELS


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
    db_session.execute(text("DELETE FROM infra_llm_models WHERE model_name LIKE 'test-reg-%'"))
    db_session.commit()
    yield
    db_session.execute(text("DELETE FROM infra_llm_models WHERE model_name LIKE 'test-reg-%'"))
    db_session.commit()


class TestGetSeedDefaults:
    def test_kimi_returns_fixed_temperature(self):
        from api.routers.models import _get_seed_defaults
        d = _get_seed_defaults("kimi-k2.5")
        assert d["quirks"]["fixed_temperature"] == 1.0

    def test_kimi_returns_strict_tool_call_ids(self):
        from api.routers.models import _get_seed_defaults
        d = _get_seed_defaults("kimi-k2.5")
        assert d["quirks"]["strict_tool_call_ids"] is True

    def test_kimi_returns_preserve_reasoning_content(self):
        from api.routers.models import _get_seed_defaults
        d = _get_seed_defaults("kimi-k2.5")
        assert d["quirks"]["preserve_reasoning_content"] is True

    def test_deepseek_has_no_quirks(self):
        from api.routers.models import _get_seed_defaults
        d = _get_seed_defaults("deepseek-chat")
        assert not d.get("quirks")  # None or missing

    def test_unknown_model_returns_empty(self):
        from api.routers.models import _get_seed_defaults
        assert _get_seed_defaults("my-custom-model") == {}

    def test_all_seed_models_with_quirks_have_fixed_temperature_type(self):
        """Any seed model with fixed_temperature must be float, not int or string."""
        from api.routers.models import _get_seed_defaults
        for sm in SEED_MODELS:
            d = _get_seed_defaults(sm["model_name"])
            ft = (d.get("quirks") or {}).get("fixed_temperature")
            if ft is not None:
                assert isinstance(ft, float), (
                    f"{sm['model_name']}.quirks.fixed_temperature must be float, got {type(ft)}"
                )


# ── POST /models auto-fills seed quirks ─────────────────────────────────────


class TestCreateModelSeedQuirks:
    """POST /models without explicit quirks must auto-fill from seed for known models."""

    def test_kimi_registration_gets_seed_quirks(self, client, admin_headers, db_session):
        """The exact bug: registering kimi-k2.5 without quirks must auto-fill all seed quirks."""
        # Use real seed model name so _get_seed_defaults finds it
        db_session.execute(text("DELETE FROM infra_llm_models WHERE model_name = 'kimi-k2.5'"))
        db_session.commit()
        resp = client.post("/models", headers=admin_headers, json={
            "name": "kimi-k2.5",
            "provider": "moonshot",
            "api_key": "sk-test",
        })
        assert resp.status_code == 201
        data = resp.json()
        assert data["quirks"]["fixed_temperature"] == 1.0, (
            f"fixed_temperature must be 1.0, got {data['quirks'].get('fixed_temperature')}"
        )
        assert data["quirks"]["strict_tool_call_ids"] is True
        assert data["quirks"]["preserve_reasoning_content"] is True

    def test_kimi_explicit_quirks_not_overridden(self, client, admin_headers, db_session):
        """Explicit quirks from caller must take precedence over seed defaults."""
        resp = client.post("/models", headers=admin_headers, json={
            "name": "test-reg-kimi3",
            "provider": "moonshot",
            "api_key": "sk-test",
            "quirks": {"fixed_temperature": 0.5},  # caller overrides
        })
        assert resp.status_code == 201

        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-reg-kimi3'")
        ).fetchone()
        quirks = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        # Caller's value wins
        assert quirks.get("fixed_temperature") == 0.5

    def test_unknown_model_gets_no_quirks_injected(self, client, admin_headers, db_session):
        """Unknown model names must not get any quirks injected."""
        resp = client.post("/models", headers=admin_headers, json={
            "name": "test-reg-custom",
            "provider": "openai",
            "api_key": "sk-test",
        })
        assert resp.status_code == 201

        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-reg-custom'")
        ).fetchone()
        quirks = row[0]
        if quirks:
            d = json.loads(quirks) if isinstance(quirks, str) else quirks
            assert d.get("fixed_temperature") is None
            assert d.get("strict_tool_call_ids") in (None, False)


# ── init_db seed sync ────────────────────────────────────────────────────────


class TestInitDbSeedSync:
    """init_db must restore quirks from SEED_MODELS on every startup."""

    def test_empty_quirks_restored_by_init_db(self, db_session):
        """If kimi-k2.5 quirks are wiped (e.g. old registration), init_db restores them."""
        # Insert kimi-k2.5 with empty quirks (simulates old registration)
        db_session.execute(text(
            "INSERT INTO infra_llm_models "
            "(model_id, model_name, provider, api_key_encrypted, is_active, quirks) "
            "VALUES (:id, 'test-reg-kimi-sync', 'moonshot', 'enc', 1, '{}')"
        ), {"id": str(uuid4())})
        db_session.commit()

        # Patch SEED_MODELS to include our test model
        import core.llm.seed_models as sm_mod
        import api.database as db_mod
        original = sm_mod.SEED_MODELS
        sm_mod.SEED_MODELS = [{"model_name": "test-reg-kimi-sync", "quirks": {
            "fixed_temperature": 1.0, "strict_tool_call_ids": True,
        }}]
        try:
            db_mod.init_db()
        finally:
            sm_mod.SEED_MODELS = original

        db_session.expire_all()
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-reg-kimi-sync'")
        ).fetchone()
        quirks = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        assert quirks.get("fixed_temperature") == 1.0
        assert quirks.get("strict_tool_call_ids") is True

    def test_null_quirks_restored_by_init_db(self, db_session):
        """NULL quirks (no column value) must also be restored by init_db."""
        db_session.execute(text(
            "INSERT INTO infra_llm_models "
            "(model_id, model_name, provider, api_key_encrypted, is_active) "
            "VALUES (:id, 'test-reg-kimi-null', 'moonshot', 'enc', 1)"
        ), {"id": str(uuid4())})
        db_session.commit()

        import core.llm.seed_models as sm_mod
        import api.database as db_mod
        original = sm_mod.SEED_MODELS
        sm_mod.SEED_MODELS = [{"model_name": "test-reg-kimi-null", "quirks": {
            "fixed_temperature": 1.0,
        }}]
        try:
            db_mod.init_db()
        finally:
            sm_mod.SEED_MODELS = original

        db_session.expire_all()
        row = db_session.execute(
            text("SELECT quirks FROM infra_llm_models WHERE model_name = 'test-reg-kimi-null'")
        ).fetchone()
        quirks = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        assert quirks.get("fixed_temperature") == 1.0


# ── ModelRegistry picks up fixed_temperature ────────────────────────────────


class TestRegistryLoadsFixedTemperature:
    """ModelRegistry.load_from_db must expose fixed_temperature via ModelConfig."""

    def test_fixed_temperature_available_on_model_config(self, db_session):
        db_session.execute(text(
            "INSERT INTO infra_llm_models "
            "(model_id, model_name, provider, api_key_encrypted, is_active, quirks) "
            "VALUES (:id, 'test-reg-load', 'moonshot', 'enc', 1, :q)"
        ), {"id": str(uuid4()), "q": json.dumps({"fixed_temperature": 1.0, "strict_tool_call_ids": True})})
        db_session.commit()

        registry = ModelRegistry()
        registry.load_from_db(db_session)

        cfg = registry.get("test-reg-load")
        assert cfg is not None
        # This is what _dispatch reads to override temperature
        assert cfg.fixed_temperature == 1.0
        assert cfg.quirks.strict_tool_call_ids is True

    def test_dispatch_uses_fixed_temperature_from_registry(self):
        """End-to-end: _dispatch must pass temperature=1.0 to provider for kimi."""
        from unittest.mock import MagicMock
        from core.llm.client import LLMClient
        from core.llm.models import LLMProvider, LLMResponse
        from core.llm.router import ModelConfig, ModelQuirks

        provider = MagicMock()
        provider.complete.return_value = LLMResponse(
            content="ok", model="kimi-k2.5", provider=LLMProvider.OPENAI,
            tokens_prompt=10, tokens_completion=5, tokens_total=15,
            latency_ms=100, cost_usd=0.0,
        )

        cfg = ModelConfig(
            model_name="kimi-k2.5", provider="moonshot",
            quirks=ModelQuirks(fixed_temperature=1.0, strict_tool_call_ids=True),
        )

        with make_test_llm_client(provider, [cfg]) as client:
            client._dispatch("kimi-k2.5", "complete", messages=[], temperature=0.7)

        _, kwargs = provider.complete.call_args
        assert kwargs["temperature"] == 1.0, f"Expected temperature=1.0, got {kwargs.get('temperature')}"


from unittest.mock import MagicMock

from tests.unit.conftest import make_test_llm_client
