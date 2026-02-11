"""Test scope-based configuration resolver."""

import pytest
from core.scope.scope_resolver import ScopeResolver, ScopeChainBuilder


class MockDB:
    """Mock database for testing."""

    def __init__(self):
        self.models = []
        self.tokens = []
        self.skills = []

    def fetchone(self, query, params):
        # Mock model query
        if "model_registry" in query:
            for model in self.models:
                if model["model_name"] == params[0] and model["scope_type"] == params[1]:
                    if len(params) == 2:
                        if model["scope_id"] is None:
                            return model
                    elif len(params) == 3:
                        if model["scope_id"] == params[2]:
                            return model

        # Mock token query
        if "api_tokens" in query:
            for token in self.tokens:
                if (
                    token["token_type"] == params[0]
                    and token["provider"] == params[1]
                    and token["scope_type"] == params[2]
                ):
                    if len(params) == 3:
                        if token["scope_id"] is None:
                            return token
                    elif len(params) == 4:
                        if token["scope_id"] == params[3]:
                            return token

        return None

    def fetchall(self, query, params):
        # Mock model list
        if "model_registry" in query:
            results = []
            for model in self.models:
                if model["scope_type"] == params[0]:
                    if len(params) == 1:
                        if model["scope_id"] is None:
                            results.append(model)
                    elif len(params) == 2:
                        if model["scope_id"] == params[1]:
                            results.append(model)
            return results

        # Mock skill list
        if "skills_registry" in query:
            results = []
            for skill in self.skills:
                if skill["scope_type"] == params[0]:
                    if len(params) == 1:
                        if skill["scope_id"] is None:
                            results.append(skill)
                    elif len(params) == 2:
                        if skill["scope_id"] == params[1]:
                            results.append(skill)
            return results

        return []


@pytest.fixture
def mock_db():
    db = MockDB()

    # Add test models
    db.models = [
        {"model_name": "gpt-4", "scope_type": "global", "scope_id": None, "provider": "openai"},
        {"model_name": "gpt-4", "scope_type": "account", "scope_id": "acme", "provider": "openai"},
        {
            "model_name": "gpt-4",
            "scope_type": "project",
            "scope_id": "backend",
            "provider": "openai",
        },
        {
            "model_name": "claude-3",
            "scope_type": "user",
            "scope_id": "alice",
            "provider": "anthropic",
        },
    ]

    # Add test tokens
    db.tokens = [
        {
            "token_type": "llm",
            "provider": "openai",
            "scope_type": "global",
            "scope_id": None,
            "encrypted_value": "global_key",
        },
        {
            "token_type": "llm",
            "provider": "openai",
            "scope_type": "account",
            "scope_id": "acme",
            "encrypted_value": "acme_key",
        },
        {
            "token_type": "llm",
            "provider": "openai",
            "scope_type": "user",
            "scope_id": "alice",
            "encrypted_value": "alice_key",
        },
    ]

    # Add test skills
    db.skills = [
        {"skill_name": "analyze_code", "scope_type": "global", "scope_id": None, "is_active": True},
        {"skill_name": "parse_logs", "scope_type": "user", "scope_id": "alice", "is_active": True},
    ]

    return db


def test_resolve_model_priority(mock_db):
    """Test model resolution follows scope priority."""
    # Build dev agent scope chain
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme", project="backend")

    resolver = ScopeResolver(mock_db, chain)

    # Should resolve to project-level config (most specific)
    model = resolver.resolve_model("gpt-4")
    assert model is not None
    assert model["scope_type"] == "project"
    assert model["scope_id"] == "backend"


def test_resolve_model_fallback(mock_db):
    """Test model resolution falls back to less specific scopes."""
    # Build chain without project
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")

    resolver = ScopeResolver(mock_db, chain)

    # Should fall back to account-level config
    model = resolver.resolve_model("gpt-4")
    assert model is not None
    assert model["scope_type"] == "account"
    assert model["scope_id"] == "acme"


def test_resolve_token_priority(mock_db):
    """Test token resolution follows scope priority."""
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")

    resolver = ScopeResolver(mock_db, chain)

    # Should resolve to user-level token (most specific in this chain)
    token = resolver.resolve_token("llm", "openai")
    assert token is not None
    assert token["scope_type"] == "user"
    assert token["encrypted_value"] == "alice_key"


def test_list_models_override(mock_db):
    """Test list_models shows most specific configs."""
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme", project="backend")

    resolver = ScopeResolver(mock_db, chain)

    models = resolver.list_models()

    # Should have 2 models: gpt-4 (project override) and claude-3 (user)
    assert len(models) == 2

    gpt4 = next(m for m in models if m["model_name"] == "gpt-4")
    assert gpt4["scope_type"] == "project"  # Most specific wins


def test_list_skills(mock_db):
    """Test list_skills returns all accessible skills."""
    chain = ScopeChainBuilder.dev_agent(user_id="alice", account_id="acme")

    resolver = ScopeResolver(mock_db, chain)

    skills = resolver.list_skills()

    # Should have 2 skills: global + user
    assert len(skills) == 2
    skill_names = [s["skill_name"] for s in skills]
    assert "analyze_code" in skill_names
    assert "parse_logs" in skill_names


def test_sales_agent_scope_chain():
    """Test sales agent scope chain builder."""
    chain = ScopeChainBuilder.sales_agent(
        user_id="bob", account_id="sales_corp", region="us-west", sales_group="enterprise"
    )

    # Verify priority order
    assert chain[0] == ("region", "us-west")
    assert chain[1] == ("sales_group", "enterprise")
    assert chain[2] == ("user", "bob")
    assert chain[3] == ("account", "sales_corp")
    assert chain[4] == ("global", None)


def test_deploy_agent_scope_chain():
    """Test deploy agent scope chain builder."""
    chain = ScopeChainBuilder.deploy_agent(
        user_id="ops", account_id="devops_team", environment="production", project="api-gateway"
    )

    # Verify priority order
    assert chain[0] == ("environment", "production")
    assert chain[1] == ("project", "api-gateway")
    assert chain[2] == ("account", "devops_team")
    assert chain[3] == ("global", None)


def test_custom_scope_chain():
    """Test custom scope chain builder."""
    chain = ScopeChainBuilder.custom(
        user_id="alice",
        account_id="acme",
        custom_scopes=[("branch", "feature-x"), ("repo", "backend")],
    )

    # Verify custom scopes come first
    assert chain[0] == ("branch", "feature-x")
    assert chain[1] == ("repo", "backend")
    assert chain[2] == ("user", "alice")
    assert chain[3] == ("account", "acme")
    assert chain[4] == ("global", None)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
