"""Tests for core/skills/scaffold.py — YAML parsing, validation, code generation."""

import pytest
import yaml

from core.skills.scaffold import SkillSpec, generate_files, _to_class_name


# ── Fixtures ─────────────────────────────────────────────────────

@pytest.fixture
def jira_yaml() -> dict:
    return {
        "name": "jira",
        "version": "1.0.0",
        "description": "Jira integration",
        "table_prefix": "sk_jira",
        "credentials": [{"name": "jira_token", "type": "secret", "required": True}],
        "tables": {
            "issues": {
                "columns": {
                    "issue_key": {"type": "string", "max_length": 20, "primary_key": True},
                    "summary": {"type": "string", "max_length": 500},
                    "status": {"type": "string", "max_length": 50},
                    "data": {"type": "json"},
                    "fetched_at": {"type": "datetime"},
                },
                "indexes": [{"columns": ["status"]}],
            }
        },
        "actions": {
            "list_issues": {
                "description": "List Jira issues",
                "parameters": {"project": {"type": "string", "required": True}},
                "side_effect": "read",
            },
        },
        "depends_on": [],
    }


@pytest.fixture
def jira_spec(jira_yaml) -> SkillSpec:
    return SkillSpec.from_dict(jira_yaml)


# ── SkillSpec.from_dict ──────────────────────────────────────────

class TestSkillSpecParsing:
    def test_valid_spec(self, jira_spec):
        assert jira_spec.name == "jira"
        assert jira_spec.version == "1.0.0"
        assert "issues" in jira_spec.tables
        assert "list_issues" in jira_spec.actions
        assert jira_spec.table_prefix == "sk_jira"

    def test_default_table_prefix(self):
        spec = SkillSpec.from_dict({"name": "foo", "version": "1.0", "description": "x"})
        assert spec.table_prefix == "sk_foo"

    def test_missing_required_field(self):
        with pytest.raises(ValueError, match="Missing required field: name"):
            SkillSpec.from_dict({"version": "1.0", "description": "x"})

    def test_invalid_skill_name(self):
        with pytest.raises(ValueError, match="Invalid skill name"):
            SkillSpec.from_dict({"name": "Bad-Name", "version": "1.0", "description": "x"})

    def test_unknown_column_type(self):
        data = {"name": "x", "version": "1.0", "description": "x",
                "tables": {"t": {"columns": {"c": {"type": "blob"}}}}}
        with pytest.raises(ValueError, match="Unknown column type"):
            SkillSpec.from_dict(data)

    def test_invalid_side_effect(self):
        data = {"name": "x", "version": "1.0", "description": "x",
                "actions": {"a": {"side_effect": "nuke"}}}
        with pytest.raises(ValueError, match="Invalid side_effect"):
            SkillSpec.from_dict(data)

    def test_invalid_table_name(self):
        data = {"name": "x", "version": "1.0", "description": "x",
                "tables": {"Bad-Table": {"columns": {}}}}
        with pytest.raises(ValueError, match="Invalid table name"):
            SkillSpec.from_dict(data)

    def test_keyword_as_identifier(self):
        data = {"name": "x", "version": "1.0", "description": "x",
                "tables": {"class": {"columns": {}}}}
        with pytest.raises(ValueError, match="Invalid table name"):
            SkillSpec.from_dict(data)

    def test_empty_tables_and_actions(self):
        spec = SkillSpec.from_dict({"name": "minimal", "version": "1.0", "description": "x"})
        assert spec.tables == {}
        assert spec.actions == {}

    def test_depends_on_parsed(self):
        data = {"name": "x", "version": "1.0", "description": "x", "depends_on": ["github", "jira"]}
        spec = SkillSpec.from_dict(data)
        assert spec.depends_on == ["github", "jira"]

    def test_action_parameter_defaults(self):
        data = {"name": "x", "version": "1.0", "description": "x",
                "actions": {"act": {"parameters": {
                    "p1": {"type": "string", "required": True},
                    "p2": {"type": "integer", "default": 10},
                    "p3": {"type": "string"},
                }}}}
        spec = SkillSpec.from_dict(data)
        params = spec.actions["act"].parameters
        assert params[0].required is True
        assert params[1].default == 10
        assert params[2].required is False

    def test_all_column_types(self):
        cols = {t: {"type": t} for t in ["string", "integer", "float", "boolean", "datetime", "json", "text"]}
        data = {"name": "x", "version": "1.0", "description": "x",
                "tables": {"t": {"columns": cols}}}
        spec = SkillSpec.from_dict(data)
        assert len(spec.tables["t"].columns) == 7


# ── Code generation ──────────────────────────────────────────────

class TestGenerateFiles:
    def test_returns_five_files(self, jira_spec):
        files = generate_files(jira_spec)
        assert set(files.keys()) == {"__init__.py", "manifest.yaml", "models.py", "api.py", "actions.py"}

    def test_all_python_files_compile(self, jira_spec):
        for fname, content in generate_files(jira_spec).items():
            if fname.endswith(".py"):
                compile(content, fname, "exec")

    def test_manifest_roundtrips(self, jira_spec):
        files = generate_files(jira_spec)
        parsed = yaml.safe_load(files["manifest.yaml"])
        assert parsed["name"] == "jira"
        assert parsed["version"] == "1.0.0"
        assert "sk_jira_issues" in parsed["tables"]

    def test_models_has_correct_class(self, jira_spec):
        models = generate_files(jira_spec)["models.py"]
        assert "class SkJiraIssues(Base):" in models
        assert '__tablename__ = "sk_jira_issues"' in models
        assert "primary_key=True" in models
        assert "Index(" in models

    def test_models_imports_correct_types(self, jira_spec):
        models = generate_files(jira_spec)["models.py"]
        assert "from sqlalchemy import" in models
        assert "JSON" in models
        assert "DateTime" in models
        assert "from api.base import Base" in models

    def test_api_has_bridge_when_depends_on(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x",
            "depends_on": ["github"],
            "actions": {"fetch": {"description": "fetch", "parameters": {}}},
        })
        api = generate_files(spec)["api.py"]
        assert "SkillDataBridge" in api
        assert "bridge:" in api

    def test_api_no_bridge_when_no_depends(self, jira_spec):
        api = generate_files(jira_spec)["api.py"]
        assert "SkillDataBridge" not in api

    def test_actions_has_correct_classes(self, jira_spec):
        actions = generate_files(jira_spec)["actions.py"]
        assert "class ListIssuesInput(SkillInput):" in actions
        assert "class ListIssuesOutput(SkillOutput):" in actions
        assert "class ListIssuesAction(Skill[ListIssuesInput, ListIssuesOutput]):" in actions
        assert 'name = "jira_list_issues"' in actions
        assert "SideEffectCategory.READ" in actions

    def test_actions_write_side_effect(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x",
            "actions": {"create": {"description": "create", "side_effect": "write"}},
        })
        actions = generate_files(spec)["actions.py"]
        assert "SideEffectCategory.WRITE" in actions

    def test_empty_tables_generates_stub(self):
        spec = SkillSpec.from_dict({"name": "empty", "version": "1.0", "description": "x"})
        models = generate_files(spec)["models.py"]
        assert "class" not in models  # no model classes

    def test_empty_actions_generates_stub(self):
        spec = SkillSpec.from_dict({"name": "empty", "version": "1.0", "description": "x"})
        actions = generate_files(spec)["actions.py"]
        assert "class" not in actions

    def test_nullable_column(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x",
            "tables": {"t": {"columns": {"c": {"type": "string", "nullable": True}}}},
        })
        models = generate_files(spec)["models.py"]
        # nullable=True means no "nullable=False" constraint
        assert "nullable=False" not in models.split("c = Column")[1].split("\n")[0]

    def test_init_has_description(self, jira_spec):
        init = generate_files(jira_spec)["__init__.py"]
        assert "Jira integration" in init


# ── End-to-end file writing ──────────────────────────────────────

class TestScaffoldEndToEnd:
    def test_write_to_disk(self, jira_spec, tmp_path):
        files = generate_files(jira_spec)
        target = tmp_path / jira_spec.name
        target.mkdir()
        for fname, content in files.items():
            (target / fname).write_text(content)

        assert (target / "manifest.yaml").exists()
        assert (target / "models.py").exists()
        assert (target / "api.py").exists()
        assert (target / "actions.py").exists()
        assert (target / "__init__.py").exists()

    def test_yaml_roundtrip(self, jira_yaml, tmp_path):
        """Write YAML, read back, scaffold, verify."""
        yaml_path = tmp_path / "skill.yaml"
        yaml_path.write_text(yaml.dump(jira_yaml))
        data = yaml.safe_load(yaml_path.read_text())
        spec = SkillSpec.from_dict(data)
        files = generate_files(spec)
        assert len(files) == 5
        for fname, content in files.items():
            if fname.endswith(".py"):
                compile(content, fname, "exec")


# ── Helpers ──────────────────────────────────────────────────────

class TestHelpers:
    @pytest.mark.parametrize("input_,expected", [
        ("sk_jira_issues", "SkJiraIssues"),
        ("list_issues", "ListIssues"),
        ("simple", "Simple"),
    ])
    def test_to_class_name(self, input_, expected):
        assert _to_class_name(input_) == expected


class TestEdgeCases:
    def test_author_in_manifest(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x", "author": "alice",
        })
        manifest = generate_files(spec)["manifest.yaml"]
        assert "alice" in manifest

    def test_action_param_with_default_in_api_and_actions(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x",
            "actions": {"act": {"description": "d", "parameters": {
                "limit": {"type": "integer", "default": 10},
            }}},
        })
        files = generate_files(spec)
        assert "= 10" in files["api.py"]
        assert "= 10" in files["actions.py"]

    def test_description_with_double_quotes(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": 'Test "quoted" desc',
            "actions": {"act": {"description": 'Do "something" here'}},
        })
        files = generate_files(spec)
        for fname, content in files.items():
            if fname.endswith(".py"):
                compile(content, fname, "exec")

    def test_description_with_backslash(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "path\\to\\file",
        })
        files = generate_files(spec)
        compile(files["__init__.py"], "__init__.py", "exec")

    def test_models_no_func_import_without_datetime(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x",
            "tables": {"t": {"columns": {"id": {"type": "integer", "primary_key": True}}}},
        })
        models = generate_files(spec)["models.py"]
        assert "from sqlalchemy.sql import func" not in models

    def test_models_has_func_import_with_datetime_at(self):
        spec = SkillSpec.from_dict({
            "name": "x", "version": "1.0", "description": "x",
            "tables": {"t": {"columns": {"created_at": {"type": "datetime"}}}},
        })
        models = generate_files(spec)["models.py"]
        assert "from sqlalchemy.sql import func" in models


# ── CLI ──────────────────────────────────────────────────────────

class TestScaffoldCLI:
    def test_scaffold_creates_files(self, jira_yaml, tmp_path):
        from click.testing import CliRunner
        from cli.mo_agent_api import cli as agent_cli

        yaml_path = tmp_path / "skill.yaml"
        yaml_path.write_text(yaml.dump(jira_yaml))
        output_dir = tmp_path / "out"
        output_dir.mkdir()

        runner = CliRunner()
        result = runner.invoke(agent_cli, [
            "skill", "scaffold", str(yaml_path), "--output-dir", str(output_dir),
        ])
        assert result.exit_code == 0
        assert "Generated skill package" in result.output
        assert (output_dir / "jira" / "manifest.yaml").exists()
        assert (output_dir / "jira" / "models.py").exists()
        assert (output_dir / "jira" / "actions.py").exists()

    def test_scaffold_refuses_overwrite(self, jira_yaml, tmp_path):
        from click.testing import CliRunner
        from cli.mo_agent_api import cli as agent_cli

        yaml_path = tmp_path / "skill.yaml"
        yaml_path.write_text(yaml.dump(jira_yaml))
        (tmp_path / "out" / "jira").mkdir(parents=True)

        runner = CliRunner()
        result = runner.invoke(agent_cli, [
            "skill", "scaffold", str(yaml_path), "--output-dir", str(tmp_path / "out"),
        ])
        assert "already exists" in result.output

    def test_scaffold_invalid_yaml(self, tmp_path):
        from click.testing import CliRunner
        from cli.mo_agent_api import cli as agent_cli

        yaml_path = tmp_path / "bad.yaml"
        yaml_path.write_text("name: Bad-Name\nversion: '1.0'\ndescription: x\n")

        runner = CliRunner()
        result = runner.invoke(agent_cli, ["skill", "scaffold", str(yaml_path)])
        assert "Error" in result.output


# ── API endpoint ─────────────────────────────────────────────────

class TestScaffoldAPI:
    def test_scaffold_endpoint_returns_files(self):
        from fastapi.testclient import TestClient
        from api.main import app

        client = TestClient(app)
        resp = client.post("/skills/scaffold", json={
            "name": "demo", "version": "1.0", "description": "Demo skill",
        })
        assert resp.status_code == 200
        data = resp.json()
        assert "__init__.py" in data
        assert "manifest.yaml" in data
        assert "models.py" in data

    def test_scaffold_endpoint_422_on_invalid(self):
        from fastapi.testclient import TestClient
        from api.main import app

        client = TestClient(app)
        resp = client.post("/skills/scaffold", json={"version": "1.0"})
        assert resp.status_code == 422
