#!/usr/bin/env python3
"""Build a repository-local inventory of static production CREATE TABLE DDL.

The goal is not to parse all SQL. It is to keep the schema audit grounded in
the Rust sources that currently create tables at startup, including schema
owners outside `storage.rs`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable


REPO_ROOT = Path(__file__).resolve().parents[2]


@dataclass(frozen=True)
class SchemaSource:
    owner: str
    domain: str
    path: str
    startup_owner: str
    state_class_hint: str
    hot_path_hint: str
    stop_at_cfg_test: bool = True


SCHEMA_SOURCES: tuple[SchemaSource, ...] = (
    SchemaSource(
        owner="astra_services::storage",
        domain="core_storage",
        path="rust/crates/services/src/storage.rs",
        startup_owner="ensure_core_schema",
        state_class_hint="mixed",
        hot_path_hint="mixed",
    ),
    SchemaSource(
        owner="astra_services::config_version_cloud",
        domain="config_versions",
        path="rust/crates/services/src/config_version_cloud.rs",
        startup_owner="ensure_core_schema via CONFIG_VERSIONS_CREATE_SQL",
        state_class_hint="durable fact",
        hot_path_hint="warm append/read",
    ),
    SchemaSource(
        owner="astra_messaging::db_transport",
        domain="messaging",
        path="rust/crates/astra-messaging/src/db_transport.rs",
        startup_owner="astra_messaging::db_transport::ensure_schema",
        state_class_hint="coordination fact",
        hot_path_hint="hot queue",
    ),
    SchemaSource(
        owner="astra_services::resource_governor",
        domain="resource_governor",
        path="rust/crates/services/src/resource_governor.rs",
        startup_owner="DatabaseResourceGovernor::ensure_tables",
        state_class_hint="quota fact",
        hot_path_hint="warm quota read/write",
    ),
    SchemaSource(
        owner="astra_services::workspace_records",
        domain="workspace_records",
        path="rust/crates/services/src/workspace_records.rs",
        startup_owner="DatabaseWorkspaceRecordStore::ensure_tables",
        state_class_hint="durable workspace fact / cleanup debt",
        hot_path_hint="run start/end workspace persistence",
    ),
    SchemaSource(
        owner="astra_runtime::llm_provider_admission",
        domain="runtime_admission",
        path="rust/crates/runtime/src/llm_provider_admission.rs",
        startup_owner="ensure_llm_provider_admission_schema_if_configured",
        state_class_hint="coordination fact",
        hot_path_hint="LLM admission hot path when enabled",
    ),
)


CREATE_TABLE_RE = re.compile(
    r"CREATE\s+TABLE\s+IF\s+NOT\s+EXISTS\s+`?([A-Za-z_][A-Za-z0-9_]*)`?\s*\(",
    re.IGNORECASE,
)

CONSTRAINT_PREFIXES = (
    "PRIMARY",
    "UNIQUE",
    "INDEX",
    "KEY",
    "CONSTRAINT",
    "CHECK",
    "FULLTEXT",
    "SPATIAL",
    "FOREIGN",
)


@dataclass
class TableInventory:
    table: str
    owner: str
    domain: str
    source_path: str
    source_line: int
    startup_owner: str
    state_class_hint: str
    hot_path_hint: str
    column_count: int
    nullable_column_count: int
    auto_increment_columns: list[str]
    primary_key: list[str]
    index_count: int
    indexes: list[str]
    has_foreign_key: bool
    ddl_sha256: str


def repository_root() -> Path:
    return REPO_ROOT


def production_source(text: str, *, stop_at_cfg_test: bool) -> str:
    if stop_at_cfg_test:
        text = text.split("#[cfg(test)]", 1)[0]
    return strip_rust_line_comments(text)


def strip_rust_line_comments(text: str) -> str:
    kept: list[str] = []
    for line in text.splitlines():
        if line.lstrip().startswith("//"):
            kept.append("")
        else:
            kept.append(line)
    return "\n".join(kept)


def find_matching_paren(text: str, open_index: int) -> int:
    depth = 0
    for index in range(open_index, len(text)):
        char = text[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index
    raise ValueError(f"unclosed CREATE TABLE body near byte {open_index}")


def split_top_level_commas(body: str) -> list[str]:
    parts: list[str] = []
    depth = 0
    start = 0
    for index, char in enumerate(body):
        if char == "(":
            depth += 1
        elif char == ")":
            depth = max(depth - 1, 0)
        elif char == "," and depth == 0:
            parts.append(body[start:index].strip())
            start = index + 1
    tail = body[start:].strip()
    if tail:
        parts.append(tail)
    return parts


def normalize_ws(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


def extract_parenthesized_columns(definition: str) -> list[str]:
    start = definition.find("(")
    if start < 0:
        return []
    end = find_matching_paren(definition, start)
    return [
        item.strip().strip("`")
        for item in split_top_level_commas(definition[start + 1 : end])
        if item.strip()
    ]


def classify_table(
    source: SchemaSource,
    table: str,
    body: str,
    source_path: str,
    source_line: int,
) -> TableInventory:
    columns: list[tuple[str, str]] = []
    indexes: list[str] = []
    primary_key: list[str] = []
    auto_increment_columns: list[str] = []

    for definition in split_top_level_commas(body):
        if not definition:
            continue
        first = definition.split(None, 1)[0].strip("`").upper()
        compact = normalize_ws(definition)
        compact_upper = f" {compact.upper()} "
        if first in CONSTRAINT_PREFIXES:
            is_primary_constraint = first == "PRIMARY" or (
                compact.upper().startswith("CONSTRAINT")
                and " PRIMARY KEY " in compact_upper
            )
            if is_primary_constraint:
                primary_key.extend(extract_parenthesized_columns(definition))
            if first in {"INDEX", "KEY", "UNIQUE"} or " INDEX " in compact_upper:
                indexes.append(compact)
            continue

        column_name = definition.split(None, 1)[0].strip("`")
        columns.append((column_name, definition))
        upper_def = f" {definition.upper()} "
        if " AUTO_INCREMENT " in upper_def:
            auto_increment_columns.append(column_name)
        if " PRIMARY KEY " in upper_def and column_name not in primary_key:
            primary_key.append(column_name)

    nullable_count = 0
    primary_key_set = set(primary_key)
    for column_name, definition in columns:
        upper_def = f" {definition.upper()} "
        if column_name in primary_key_set or " NOT NULL " in upper_def:
            continue
        nullable_count += 1

    ddl_text = f"CREATE TABLE IF NOT EXISTS {table} ({body})"
    return TableInventory(
        table=table,
        owner=source.owner,
        domain=source.domain,
        source_path=source_path,
        source_line=source_line,
        startup_owner=source.startup_owner,
        state_class_hint=source.state_class_hint,
        hot_path_hint=source.hot_path_hint,
        column_count=len(columns),
        nullable_column_count=nullable_count,
        auto_increment_columns=auto_increment_columns,
        primary_key=primary_key,
        index_count=len(indexes),
        indexes=indexes,
        has_foreign_key=bool(re.search(r"\b(FOREIGN\s+KEY|REFERENCES)\b", body, re.IGNORECASE)),
        ddl_sha256=hashlib.sha256(normalize_ws(ddl_text).encode("utf-8")).hexdigest(),
    )


def extract_tables_from_source(root: Path, source: SchemaSource) -> list[TableInventory]:
    path = root / source.path
    text = path.read_text(encoding="utf-8")
    text = production_source(text, stop_at_cfg_test=source.stop_at_cfg_test)
    tables: list[TableInventory] = []
    for match in CREATE_TABLE_RE.finditer(text):
        table = match.group(1)
        open_index = match.end() - 1
        close_index = find_matching_paren(text, open_index)
        body = text[open_index + 1 : close_index]
        source_line = text.count("\n", 0, match.start()) + 1
        tables.append(classify_table(source, table, body, source.path, source_line))
    return tables


def build_inventory(root: Path | None = None) -> dict[str, object]:
    root = root or REPO_ROOT
    tables: list[TableInventory] = []
    for source in SCHEMA_SOURCES:
        tables.extend(extract_tables_from_source(root, source))

    by_name: dict[str, list[TableInventory]] = {}
    for table in tables:
        by_name.setdefault(table.table, []).append(table)

    duplicates = {
        name: [f"{entry.source_path}:{entry.source_line}" for entry in entries]
        for name, entries in sorted(by_name.items())
        if len(entries) > 1
    }
    auto_increment_tables = sorted(
        table.table for table in tables if table.auto_increment_columns
    )
    foreign_key_tables = sorted(table.table for table in tables if table.has_foreign_key)
    by_domain: dict[str, int] = {}
    for table in tables:
        by_domain[table.domain] = by_domain.get(table.domain, 0) + 1

    return {
        "schema_sources": [asdict(source) for source in SCHEMA_SOURCES],
        "summary": {
            "source_count": len(SCHEMA_SOURCES),
            "table_declaration_count": len(tables),
            "unique_table_count": len(by_name),
            "duplicate_table_names": duplicates,
            "foreign_key_tables": foreign_key_tables,
            "auto_increment_tables": auto_increment_tables,
            "domain_table_counts": dict(sorted(by_domain.items())),
        },
        "tables": [asdict(table) for table in sorted(tables, key=lambda item: item.table)],
    }


def render_markdown(inventory: dict[str, object]) -> str:
    summary = inventory["summary"]
    assert isinstance(summary, dict)
    lines = [
        "# Schema Inventory",
        "",
        "Generated from repository Rust DDL sources.",
        "",
        "## Summary",
        "",
        f"- Sources: {summary['source_count']}",
        f"- Table declarations: {summary['table_declaration_count']}",
        f"- Unique tables: {summary['unique_table_count']}",
        f"- Duplicate table names: {len(summary['duplicate_table_names'])}",
        f"- Foreign-key tables: {len(summary['foreign_key_tables'])}",
        f"- AUTO_INCREMENT tables: {len(summary['auto_increment_tables'])}",
        "",
        "## Tables",
        "",
        "| Table | Domain | Owner | Source | Columns | Nullable | AUTO_INCREMENT | PK | Indexes |",
        "|---|---|---|---|---:|---:|---|---|---:|",
    ]
    for table in inventory["tables"]:
        assert isinstance(table, dict)
        auto_inc = ", ".join(table["auto_increment_columns"]) or "-"
        pk = ", ".join(table["primary_key"]) or "-"
        lines.append(
            "| `{table}` | `{domain}` | `{owner}` | `{source_path}:{source_line}` | "
            "{column_count} | {nullable_column_count} | `{auto_inc}` | `{pk}` | {index_count} |".format(
                auto_inc=auto_inc,
                pk=pk,
                **table,
            )
        )
    return "\n".join(lines) + "\n"


def write_output(text: str, output: Path | None) -> None:
    if output is None:
        sys.stdout.write(text)
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text, encoding="utf-8")


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--fail-on-duplicates", action="store_true")
    parser.add_argument("--fail-on-foreign-keys", action="store_true")
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] = sys.argv[1:]) -> int:
    args = parse_args(argv)
    inventory = build_inventory(args.repo_root)
    summary = inventory["summary"]
    assert isinstance(summary, dict)
    if args.fail_on_duplicates and summary["duplicate_table_names"]:
        print(
            f"duplicate table declarations: {summary['duplicate_table_names']}",
            file=sys.stderr,
        )
        return 2
    if args.fail_on_foreign_keys and summary["foreign_key_tables"]:
        print(f"foreign-key tables: {summary['foreign_key_tables']}", file=sys.stderr)
        return 2

    if args.format == "json":
        text = json.dumps(inventory, indent=2, sort_keys=True) + "\n"
    else:
        text = render_markdown(inventory)
    write_output(text, args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
