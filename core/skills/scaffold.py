"""Skill scaffold — parse YAML spec, generate skill package files.

Pure logic, zero I/O. Callers (CLI, API, IDE plugin) handle file reading/writing.
"""

from __future__ import annotations

import keyword
import re
from dataclasses import dataclass, field
from typing import Any

# ── Type mapping ──────────────────────────────────────────────────

_TYPE_MAP: dict[str, dict[str, str]] = {
    "string":   {"sqlalchemy": "String({max_length})", "python": "str"},
    "integer":  {"sqlalchemy": "Integer",              "python": "int"},
    "float":    {"sqlalchemy": "Float",                "python": "float"},
    "boolean":  {"sqlalchemy": "SmallInteger",         "python": "bool"},
    "datetime": {"sqlalchemy": "DateTime",             "python": "datetime"},
    "json":     {"sqlalchemy": "JSON",                 "python": "dict"},
    "text":     {"sqlalchemy": "Text",                 "python": "str"},
}

_VALID_SIDE_EFFECTS = {"read", "write", "execute", "destructive"}
_IDENTIFIER_RE = re.compile(r"^[a-z][a-z0-9_]*$")


# ── Spec dataclasses ─────────────────────────────────────────────

@dataclass
class ColumnSpec:
    name: str
    type: str
    max_length: int = 255
    primary_key: bool = False
    nullable: bool = False


@dataclass
class TableSpec:
    name: str
    columns: list[ColumnSpec] = field(default_factory=list)
    indexes: list[list[str]] = field(default_factory=list)


@dataclass
class ParameterSpec:
    name: str
    type: str
    required: bool = False
    default: Any = None
    enum: list[str] | None = None


@dataclass
class ActionSpec:
    name: str
    description: str = ""
    parameters: list[ParameterSpec] = field(default_factory=list)
    side_effect: str = "read"


@dataclass
class SkillSpec:
    name: str
    version: str
    description: str
    table_prefix: str
    credentials: list[dict[str, Any]]
    tables: dict[str, TableSpec]
    actions: dict[str, ActionSpec]
    depends_on: list[str | dict[str, str]]
    author: str = ""
    requires: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SkillSpec:
        """Validate and construct from parsed YAML dict. Raises ValueError."""
        # Required fields
        for f in ("name", "version", "description"):
            if f not in data:
                raise ValueError(f"Missing required field: {f}")

        name = data["name"]
        if not _IDENTIFIER_RE.match(name):
            raise ValueError(f"Invalid skill name: {name!r} (must be lowercase identifier)")

        prefix = data.get("table_prefix", f"sk_{name}")

        # Parse tables
        tables: dict[str, TableSpec] = {}
        for tname, tdef in (data.get("tables") or {}).items():
            _validate_identifier(tname, "table")
            cols = []
            for cname, cdef in (tdef.get("columns") or {}).items():
                _validate_identifier(cname, "column")
                ctype = cdef.get("type", "string")
                if ctype not in _TYPE_MAP:
                    raise ValueError(f"Unknown column type: {ctype!r} in {tname}.{cname}")
                cols.append(ColumnSpec(
                    name=cname, type=ctype,
                    max_length=cdef.get("max_length", 255),
                    primary_key=cdef.get("primary_key", False),
                    nullable=cdef.get("nullable", False),
                ))
            indexes = [idx["columns"] for idx in (tdef.get("indexes") or [])]
            tables[tname] = TableSpec(name=tname, columns=cols, indexes=indexes)

        # Parse actions
        actions: dict[str, ActionSpec] = {}
        for aname, adef in (data.get("actions") or {}).items():
            _validate_identifier(aname, "action")
            se = adef.get("side_effect", "read")
            if se not in _VALID_SIDE_EFFECTS:
                raise ValueError(f"Invalid side_effect: {se!r} in action {aname}")
            params = []
            for pname, pdef in (adef.get("parameters") or {}).items():
                _validate_identifier(pname, "parameter")
                params.append(ParameterSpec(
                    name=pname, type=pdef.get("type", "string"),
                    required=pdef.get("required", False),
                    default=pdef.get("default"),
                    enum=pdef.get("enum"),
                ))
            actions[aname] = ActionSpec(
                name=aname, description=adef.get("description", ""),
                parameters=params, side_effect=se,
            )

        return cls(
            name=name, version=data["version"], description=data["description"],
            table_prefix=prefix, credentials=data.get("credentials", []),
            tables=tables, actions=actions,
            depends_on=data.get("depends_on", []),
            author=data.get("author", ""),
            requires=data.get("requires", []),
        )


def _validate_identifier(name: str, kind: str) -> None:
    if not _IDENTIFIER_RE.match(name) or keyword.iskeyword(name):
        raise ValueError(f"Invalid {kind} name: {name!r}")


# ── Code generators ──────────────────────────────────────────────

def generate_files(spec: SkillSpec) -> dict[str, str]:
    """Return {filename: content} for all skill package files. No disk I/O."""
    return {
        "__init__.py": _gen_init(spec),
        "manifest.yaml": _gen_manifest(spec),
        "models.py": _gen_models(spec),
        "api.py": _gen_api(spec),
        "actions.py": _gen_actions(spec),
    }


def _gen_init(spec: SkillSpec) -> str:
    return f'"""{_escape(spec.description)}"""\n'


def _gen_manifest(spec: SkillSpec) -> str:
    import yaml
    data: dict[str, Any] = {
        "name": spec.name,
        "version": spec.version,
        "description": spec.description,
    }
    if spec.author:
        data["author"] = spec.author
    data["table_prefix"] = spec.table_prefix
    data["tables"] = [f"{spec.table_prefix}_{t}" for t in spec.tables]
    data["credentials"] = spec.credentials
    data["requires"] = spec.requires
    data["depends_on"] = spec.depends_on
    return yaml.dump(data, default_flow_style=False, sort_keys=False)


def _gen_models(spec: SkillSpec) -> str:
    if not spec.tables:
        return f'"""{spec.name} skill tables."""\n'

    # Collect needed SQLAlchemy imports
    sa_types = {"Column", "String"}  # String always needed for primary keys
    needs_func = False
    for table in spec.tables.values():
        for col in table.columns:
            mapping = _TYPE_MAP[col.type]
            sa_type = mapping["sqlalchemy"].split("(")[0]
            sa_types.add(sa_type)
            if col.type == "datetime" and col.name.endswith("_at"):
                needs_func = True
        if table.indexes:
            sa_types.add("Index")

    sa_imports = ", ".join(sorted(sa_types))
    lines = [
        f'"""{spec.name} skill tables — platform DB with {spec.table_prefix}_ prefix."""',
        "",
        f"from sqlalchemy import {sa_imports}",
    ]
    if needs_func:
        lines.append("from sqlalchemy.sql import func")
    lines += [
        "",
        "from api.base import Base",
    ]

    for tname, table in spec.tables.items():
        full_table = f"{spec.table_prefix}_{tname}"
        class_name = _to_class_name(full_table)
        lines += ["", "", f"class {class_name}(Base):", f'    __tablename__ = "{full_table}"']

        # Index table_args
        if table.indexes:
            idx_parts = []
            for idx_cols in table.indexes:
                cols_str = ", ".join(f'"{c}"' for c in idx_cols)
                idx_name = f"ix_{full_table}_{'_'.join(idx_cols)}"
                idx_parts.append(f'        Index("{idx_name}", {cols_str}),')
            lines.append("    __table_args__ = (")
            lines.extend(idx_parts)
            lines.append("    )")

        lines.append("")
        for col in table.columns:
            lines.append(f"    {_col_line(col)}")

    lines.append("")
    return "\n".join(lines)


def _col_line(col: ColumnSpec) -> str:
    mapping = _TYPE_MAP[col.type]
    sa_raw = mapping["sqlalchemy"]
    sa_type = sa_raw.format(max_length=col.max_length) if "{max_length}" in sa_raw else sa_raw

    parts = [f"Column({sa_type}"]
    if col.primary_key:
        parts.append(", primary_key=True")
    if not col.primary_key and not col.nullable:
        parts.append(", nullable=False")
    if col.type == "datetime" and col.name.endswith("_at"):
        parts.append(", default=func.now()")
    parts.append(")")
    return f"{col.name} = {''.join(parts)}"


def _gen_api(spec: SkillSpec) -> str:
    class_name = _to_class_name(spec.name) + "SkillAPI"
    lines = [
        f'"""{_escape(spec.name)} skill API — typed interface for data access."""',
        "",
        "from __future__ import annotations",
        "",
        "from sqlalchemy.orm import Session",
        "",
        "from core.logging_config import get_logger",
    ]

    if spec.depends_on:
        lines.append("from core.skills.data_bridge import SkillDataBridge")

    lines += [
        "",
        f"logger = get_logger(__name__)",
        "",
        "",
        f"class {class_name}:",
        f'    """{_escape(spec.description)}"""',
        "",
    ]

    # Constructor
    init_params = ["self", "db: Session | None = None"]
    if spec.depends_on:
        init_params.append("bridge: SkillDataBridge | None = None")
    lines.append(f"    def __init__({', '.join(init_params)}):")
    lines.append("        self._db = db")
    if spec.depends_on:
        lines.append("        self._bridge = bridge")
    lines.append("")

    # Stub methods per action
    for aname, action in spec.actions.items():
        params = ["self"]
        for p in action.parameters:
            py_type = _TYPE_MAP.get(p.type, {}).get("python", "str")
            if p.default is not None:
                params.append(f"{p.name}: {py_type} = {p.default!r}")
            elif not p.required:
                params.append(f"{p.name}: {py_type} | None = None")
            else:
                params.append(f"{p.name}: {py_type}")
        sig = ", ".join(params)
        lines += [
            f"    async def {aname}({sig}) -> dict:",
            f'        """{_escape(action.description)}"""',
            "        raise NotImplementedError",
            "",
        ]

    return "\n".join(lines)


def _gen_actions(spec: SkillSpec) -> str:
    if not spec.actions:
        return f'"""{spec.name} skill actions."""\n'

    api_class = _to_class_name(spec.name) + "SkillAPI"
    lines = [
        f'"""{spec.name} skill actions — registered as tools for the agent."""',
        "",
        "from __future__ import annotations",
        "",
        "from core.skills.base import (",
        "    SideEffectCategory,",
        "    SideEffectProfile,",
        "    Skill,",
        "    SkillInput,",
        "    SkillOutput,",
        "    SkillRequirement,",
        ")",
        f"from skills.{spec.name}.api import {api_class}",
    ]

    for aname, action in spec.actions.items():
        input_cls = _to_class_name(aname) + "Input"
        output_cls = _to_class_name(aname) + "Output"
        action_cls = _to_class_name(aname) + "Action"
        se_upper = action.side_effect.upper()

        lines += [
            "",
            "",
            f"class {input_cls}(SkillInput):",
        ]
        if action.parameters:
            for p in action.parameters:
                py_type = _TYPE_MAP.get(p.type, {}).get("python", "str")
                if p.default is not None:
                    lines.append(f"    {p.name}: {py_type} = {p.default!r}")
                elif not p.required:
                    lines.append(f"    {p.name}: {py_type} | None = None")
                else:
                    lines.append(f"    {p.name}: {py_type}")
        else:
            lines.append("    pass")

        lines += [
            "",
            "",
            f"class {output_cls}(SkillOutput):",
            "    data: dict | None = None",
            "",
            "",
            f"class {action_cls}(Skill[{input_cls}, {output_cls}]):",
            f'    name = "{spec.name}_{aname}"',
            f'    version = "{spec.version}"',
            f'    description = "{_escape(action.description)}"',
            f"    requirements = SkillRequirement()",
            f"    side_effect_profile = SideEffectProfile(",
            f"        category=SideEffectCategory.{se_upper},",
            f"    )",
            "",
            f"    def __init__(self, api: {api_class}):",
            f"        self._api = api",
            "",
            f"    async def execute(self, input: {input_cls}) -> {output_cls}:",
            f"        result = await self._api.{aname}(",
        ]
        if action.parameters:
            for p in action.parameters:
                lines.append(f"            {p.name}=input.{p.name},")
        lines += [
            f"        )",
            f"        return {output_cls}(success=True, result=result, data=result)",
        ]

    lines.append("")
    return "\n".join(lines)


# ── Helpers ──────────────────────────────────────────────────────

def _to_class_name(snake: str) -> str:
    """Convert snake_case or prefixed name to PascalCase. e.g. sk_jira_issues → SkJiraIssues."""
    return "".join(w.capitalize() for w in snake.split("_"))


def _escape(s: str) -> str:
    """Escape a string for safe embedding in generated Python string literals."""
    return s.replace("\\", "\\\\").replace('"', '\\"')
