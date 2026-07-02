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
    semantic_owner: str
    state_class: str
    primary_query: str
    retention_policy: str
    rebuildability: str
    merge_guidance: str
    migration_owner: str
    product_owner: str
    auto_increment_write_profile: str
    auto_increment_owner_boundary: str
    auto_increment_hotspot_risk: str
    auto_increment_guidance: str


@dataclass(frozen=True)
class TableMetadata:
    semantic_owner: str
    state_class: str
    primary_query: str
    retention_policy: str
    rebuildability: str
    merge_guidance: str
    migration_owner: str
    product_owner: str


@dataclass(frozen=True)
class AutoIncrementMetadata:
    write_profile: str
    owner_boundary: str
    hotspot_risk: str
    guidance: str


DEFAULT_METADATA = TableMetadata(
    semantic_owner="unclassified",
    state_class="unclassified",
    primary_query="unclassified",
    retention_policy="unclassified",
    rebuildability="unclassified",
    merge_guidance="unclassified",
    migration_owner="unclassified",
    product_owner="unclassified",
)


DEFAULT_AUTO_INCREMENT_METADATA = AutoIncrementMetadata(
    write_profile="not_applicable",
    owner_boundary="not_applicable",
    hotspot_risk="not_applicable",
    guidance="not_applicable",
)


AUTO_INCREMENT_METADATA: dict[str, AutoIncrementMetadata] = {}


TABLE_METADATA: dict[str, TableMetadata] = {
    "agent_events": TableMetadata(
        semantic_owner="astra_services::events / event_ingestion",
        state_class="durable audit timeline",
        primary_query="session timeline by user_id, session_id, created_at and event_type",
        retention_policy="high-growth audit fact; retain until session audit, memory, reflect, and evaluation consumers no longer need the timeline; event TTL and session hard delete prune in ordered bounded batches",
        rebuildability="not rebuildable as a full audit timeline",
        merge_guidance="do not merge into agent_run_events; this is the cross-run session audit timeline",
        migration_owner="astra_services::storage",
        product_owner="session audit, introspection, memory, evaluation",
    ),
    "agent_run_events": TableMetadata(
        semantic_owner="astra_services::runs::DatabaseRunStateStore",
        state_class="durable run replay fact",
        primary_query="ordered run replay and projection repair by user_id, session_id, run_id, event_idx",
        retention_policy="no independent age-based TTL: retain while run replay, projection repair, and recovery may need the run boundary events; session reaper invokes session hard delete to prune owner/session rows in ordered bounded batches",
        rebuildability="not rebuildable from agent_events without losing event_idx and replay ordering",
        merge_guidance="do not merge into agent_events; this owns per-run monotonic replay ordering",
        migration_owner="astra_services::runs",
        product_owner="run replay, projection repair, runtime recovery",
    ),
    "conversation_log": TableMetadata(
        semantic_owner="astra_turn_core::conversation_log::db_store",
        state_class="durable model context state log",
        primary_query="latest compact/snapshot and turn deltas by user_id, session_id, seq",
        retention_policy="retain while model context reconstruction for the session is required; runtime compaction truncates old entries in ordered bounded batches and session hard delete removes owner/session rows",
        rebuildability="partially rebuildable only when all source transcript/history events remain",
        merge_guidance="do not merge with transcript or audit events; it serves model-context reconstruction",
        migration_owner="astra_turn_core::conversation_log",
        product_owner="LLM context assembly and session continuity",
    ),
    "session_transcript_items": TableMetadata(
        semantic_owner="runtime run lifecycle persistence",
        state_class="durable user-visible transcript stream",
        primary_query="transcript hydration by user_id, session_id, item_seq and run/event lookup",
        retention_policy="retain with user-visible session history; do not delete before transcript pages/history are reconciled",
        rebuildability="partially rebuildable for new writes, but item sequence and page identity are product state",
        merge_guidance="do not merge with conversation_log; transcript is display state, not model context",
        migration_owner="astra_services::storage / runtime session persistence",
        product_owner="web/session transcript UI",
    ),
    "transcript_pages": TableMetadata(
        semantic_owner="runtime run lifecycle persistence",
        state_class="derived transcript projection",
        primary_query="transcript pagination by user_id, session_id, page_seq or end item sequence",
        retention_policy="can be cleared and rebuilt from session_transcript_items with a repair path",
        rebuildability="rebuildable from session_transcript_items",
        merge_guidance="candidate for repair tooling, not deletion; merge only if pagination cost is proven low",
        migration_owner="astra_services::storage / runtime session persistence",
        product_owner="web/session transcript pagination",
    ),
    "session_history_chunks": TableMetadata(
        semantic_owner="astra_services::session_lifecycle / history search",
        state_class="derived search/history projection",
        primary_query="history chunk lookup by owner/session/source metadata",
        retention_policy="retain while history search and session cleanup need chunk metadata",
        rebuildability="rebuildable only if source transcript/history remains complete",
        merge_guidance="do not merge with transcript until search/query owners and retention are unified",
        migration_owner="astra_services::storage",
        product_owner="session history search and lifecycle cleanup",
    ),
    "prompt_request_records": TableMetadata(
        semantic_owner="astra_services::prompt_delta",
        state_class="durable prompt observability parent fact",
        primary_query="latest prompt request by user_id/session_id or user_id/run_id ordered by created_at/turn/round/attempt; parent lookup by user_id/request_id",
        retention_policy="retain with prompt_deltas while prompt diff observability for the session/run is needed; age-based cleanup uses created_at_unix_ms retention key to avoid MatrixOne DATETIME cast scans, selects parent rows only after session is inactive and run is terminal, then prunes child deltas before parent rows in bounded chunks; session hard delete prunes parent rows after child deltas in ordered bounded batches",
        rebuildability="partially rebuildable only if the rendered prompt inputs and chunk hashes still exist",
        merge_guidance="keep as the parent fact for prompt_deltas; do not delete child deltas independently",
        migration_owner="astra_services::storage / prompt_delta",
        product_owner="prompt observability, prompt diffing, context assembly diagnostics",
    ),
    "prompt_deltas": TableMetadata(
        semantic_owner="astra_services::prompt_delta",
        state_class="derived prompt delta projection",
        primary_query="request chunk diff lookup by user_id/session_id/request_id ordered by position/delta_seq",
        retention_policy="delete with parent prompt_request_records during age-based cleanup or session deletion; independent TTL breaks previous-request diff lineage; retention cleanup and session hard delete prune child rows before prompt_request_records in bounded batches",
        rebuildability="rebuildable only while consecutive prompt request payloads and chunk hashes remain available",
        merge_guidance="keep separate from prompt_request_records because chunk cardinality grows per prompt and must be pruned with its parent",
        migration_owner="astra_services::storage / prompt_delta",
        product_owner="prompt diffing and LLM request diagnostics",
    ),
    "session_tool_output_batches": TableMetadata(
        semantic_owner="astra_services::runs::insert_tool_output_batch",
        state_class="durable tool-output batch metadata",
        primary_query="batch commit metadata by user_id, session_id, batch_id",
        retention_policy="no independent age-based TTL: retain with tool outputs; session reaper invokes session hard delete to prune owner/session rows in ordered bounded batches after output rows",
        rebuildability="rebuildable only if every individual output row remains intact",
        merge_guidance="keep separate from session_tool_outputs; it bounds batch writes and records batch status/bytes",
        migration_owner="astra_services::runs",
        product_owner="tool output persistence and audit",
    ),
    "session_tool_outputs": TableMetadata(
        semantic_owner="astra_services::runs::insert_tool_output_batch",
        state_class="durable large payload fact",
        primary_query="tool output lookup by user_id, session_id, output_id and batch order",
        retention_policy="no independent age-based TTL: high-growth payload table is not rebuildable and may be referenced by previews/artifact refs; session reaper invokes session hard delete to prune owner/session rows in ordered bounded batches and cleanup must still respect unfinished runs",
        rebuildability="not rebuildable once external or ephemeral tool output is gone",
        merge_guidance="do not merge into agent_run_events; large payload retention and indexing are separate concerns",
        migration_owner="astra_services::runs",
        product_owner="tool output history, previews, and artifacts",
    ),
    "tool_exactly_once_results": TableMetadata(
        semantic_owner="runtime::server::tool_exactly_once",
        state_class="coordination fact",
        primary_query="side-effect dedup lookup by user_id, session_id, dedup_key",
        retention_policy="retain at least as long as side-effect replay is possible for the session/run",
        rebuildability="not rebuildable safely; prevents duplicate side effects after crash or retry",
        merge_guidance="never merge into event logs; exactly-once lookup must stay direct and small",
        migration_owner="runtime::server::tool_exactly_once",
        product_owner="tool side-effect safety",
    ),
    "agent_message_queue": TableMetadata(
        semantic_owner="astra_messaging::db_transport",
        state_class="coordination queue fact",
        primary_query="claim pending direct messages by recipient/status/created_at/message_id; fetch undelivered broadcasts by delegation/status/created_at/message_id",
        retention_policy="TTL/cleanup governed queue; expired and terminal messages are pruned in ordered bounded batches and orphan broadcast delivery rows are pruned after queue deletion",
        rebuildability="not rebuildable for pending messages",
        merge_guidance="keep outside run/event tables; queue claim, visibility timeout, and retry semantics are distinct",
        migration_owner="astra_messaging::db_transport",
        product_owner="distributed agent messaging",
    ),
    "agent_message_broadcast_delivery": TableMetadata(
        semantic_owner="astra_messaging::db_transport",
        state_class="coordination delivery fact",
        primary_query="per-consumer broadcast dedup lookup by message_id and consumer_id",
        retention_policy="remove orphan delivery rows in ordered bounded batches after parent queue messages are pruned",
        rebuildability="rebuildable only by allowing broadcast redelivery to active consumers",
        merge_guidance="keep separate from agent_message_queue; it replaces global queue cursor state with consumer-scoped delivery state",
        migration_owner="astra_messaging::db_transport",
        product_owner="distributed agent messaging",
    ),
    "resource_limits": TableMetadata(
        semantic_owner="astra_services::resource_governor",
        state_class="quota fact",
        primary_query="per-user quota override by user_id",
        retention_policy="retain while user override is active; delete means fall back to defaults",
        rebuildability="rebuildable only from admin/product configuration source if one exists",
        merge_guidance="keep separate from usage counters; limits and usage have different lifecycles",
        migration_owner="astra_services::resource_governor",
        product_owner="admin quota controls",
    ),
    "resource_usage": TableMetadata(
        semantic_owner="astra_services::resource_governor",
        state_class="quota usage counter",
        primary_query="daily usage counter by user_id and usage_date",
        retention_policy="retain for quota window and reporting; old windows can be batch-pruned",
        rebuildability="not fully rebuildable unless all source session/tool/token events remain complete",
        merge_guidance="keep separate from resource_limits; usage is high-churn by day",
        migration_owner="astra_services::resource_governor",
        product_owner="quota enforcement and reporting",
    ),
    "llm_provider_admission_windows": TableMetadata(
        semantic_owner="astra_runtime::llm_provider_admission",
        state_class="coordination rate-limit window",
        primary_query="provider/provider-model fixed window claim by bucket_key and window_start_ms",
        retention_policy="short rolling retention controlled by admission retention windows and cleanup interval",
        rebuildability="not rebuildable for current admission windows, but old windows can expire",
        merge_guidance="keep separate from durable audit; this is a hot coordination table",
        migration_owner="astra_runtime::llm_provider_admission",
        product_owner="LLM provider admission control",
    ),
    "edge_pending_dispatch": TableMetadata(
        semantic_owner="astra_services::multi_agent::edge_dispatch",
        state_class="coordination dispatch fact",
        primary_query="owner/request dispatch lookup by user_id and request_id; edge poll by user_id, edge_agent_id, status, created_at",
        retention_policy="expire pending/dispatched rows via cleanup_stale and prune terminal rows after the same stale window",
        rebuildability="not rebuildable while a tool dispatch may still complete",
        merge_guidance="keep separate from in-memory edge callback ledger; this is the cross-pod durable coordination queue",
        migration_owner="astra_services::storage / multi_agent::edge_dispatch",
        product_owner="edge tool dispatch and cross-pod result recovery",
    ),
    "context_manifest_items": TableMetadata(
        semantic_owner="astra_services::context_manifest::ContextManifestStore",
        state_class="durable manifest item fact",
        primary_query="manifest-local item lookup/order by manifest_id and item_order; zone filtering by manifest_id, zone, included",
        retention_policy="delete with parent context_manifests during session deletion or manifest retention cleanup",
        rebuildability="rebuildable only by recomputing the exact manifest assembly inputs",
        merge_guidance="keep separate from context_manifests; item-level ordering, source refs, token budgets, and raw refs are independently queried",
        migration_owner="astra_services::storage / context_manifest",
        product_owner="LLM context assembly traceability",
    ),
    "session_state_item_events": TableMetadata(
        semantic_owner="astra_services::state_projection",
        state_class="durable state projection audit event",
        primary_query="state item audit by item_id/created_at/event_id and owner session audit by user_id/session_id/created_at/event_id",
        retention_policy="retain with session_state_items while compaction invariants, projection debugging, and user/session audit need mutation history",
        rebuildability="not fully rebuildable after source mutation context is gone",
        merge_guidance="keep separate from session_state_items; the item table is current projection state, while this table records mutation history",
        migration_owner="astra_services::storage / state_projection",
        product_owner="session state projection, compaction invariants, active skill/delegation audit",
    ),
    "session_sync_log": TableMetadata(
        semantic_owner="astra_services::state_sync::SyncAuditWriter",
        state_class="best-effort sync observability audit",
        primary_query="sync status and latest error by status/created_at, with owner/session deletion by user_id/session_id",
        retention_policy="Storage cleanup prunes old rows by sync_log_days default 30 in ordered batches; session deletion removes owner/session rows",
        rebuildability="not required for business correctness and not exactly rebuildable after audit rows are dropped",
        merge_guidance="do not replace with tracing until sync_status readers stop querying this product audit table",
        migration_owner="astra_services::storage / state_sync",
        product_owner="session sync status, operational diagnostics",
    ),
    "auth_user_roles": TableMetadata(
        semantic_owner="astra_services::auth::admin / auth registration",
        state_class="durable auth grant fact",
        primary_query="role membership lookup and grant/revoke by user_id and role_id; admin role member lookup by role_id",
        retention_policy="retain while the user-role grant is active; delete is revoke",
        rebuildability="rebuildable only from an external auth/identity source if one exists",
        merge_guidance="keep separate from auth_users/auth_roles; this is the many-to-many grant fact with independent revoke lifecycle",
        migration_owner="astra_services::storage / auth",
        product_owner="auth and admin role management",
    ),
    "auth_external_identities": TableMetadata(
        semantic_owner="astra_services::auth",
        state_class="durable external identity link fact",
        primary_query="external login lookup by provider_id and external_subject; user lookup by astra_user_id",
        retention_policy="retain while the external identity remains linked to an astra user",
        rebuildability="not rebuildable after provider link metadata is lost",
        merge_guidance="keep separate from auth_external_sessions; identity link outlives individual provider sessions",
        migration_owner="astra_services::storage / auth",
        product_owner="external auth login and account linking",
    ),
    "mcp_servers": TableMetadata(
        semantic_owner="astra_services::mcp_registry",
        state_class="durable MCP server registry fact",
        primary_query="owner server registration by owner_user_id/name and runtime join by owner_user_id/id",
        retention_policy="retain while the owner keeps the MCP server active; inactive rows may be pruned after dependent bindings are removed",
        rebuildability="rebuildable only from owner MCP registration configuration if that source still exists",
        merge_guidance="keep separate from mcp_bindings; server endpoint lifecycle differs from credentials",
        migration_owner="astra_services::storage / mcp_registry",
        product_owner="MCP registry and runtime tool discovery",
    ),
    "mcp_bindings": TableMetadata(
        semantic_owner="astra_services::mcp_registry",
        state_class="durable MCP credential binding fact",
        primary_query="owner binding lookup by owner_user_id/id and dedup by owner_user_id/mcp_id/key_hash",
        retention_policy="retain while credential binding is active; delete/revoke must also remove discovered mcp_tools",
        rebuildability="not rebuildable after encrypted credential payload is lost",
        merge_guidance="keep separate from mcp_servers and mcp_tools; credentials have independent lifecycle and secrecy requirements",
        migration_owner="astra_services::storage / mcp_registry",
        product_owner="MCP credential binding and runtime execution",
    ),
    "mcp_tools": TableMetadata(
        semantic_owner="astra_services::mcp_registry",
        state_class="derived MCP discovery projection",
        primary_query="binding tool hydration by owner_user_id/binding_id ordered by public_name",
        retention_policy="replace atomically on discovery refresh; delete with parent binding",
        rebuildability="rebuildable by rediscovering tools from the MCP server when credentials and endpoint remain valid",
        merge_guidance="keep separate from bindings; tool schema fanout is a replaceable projection with different row cardinality",
        migration_owner="astra_services::storage / mcp_registry",
        product_owner="MCP runtime tool surface",
    ),
}


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
    metadata = TABLE_METADATA.get(table, DEFAULT_METADATA)
    auto_increment_metadata = (
        AUTO_INCREMENT_METADATA.get(table, DEFAULT_AUTO_INCREMENT_METADATA)
        if auto_increment_columns
        else DEFAULT_AUTO_INCREMENT_METADATA
    )
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
        semantic_owner=metadata.semantic_owner,
        state_class=metadata.state_class,
        primary_query=metadata.primary_query,
        retention_policy=metadata.retention_policy,
        rebuildability=metadata.rebuildability,
        merge_guidance=metadata.merge_guidance,
        migration_owner=metadata.migration_owner,
        product_owner=metadata.product_owner,
        auto_increment_write_profile=auto_increment_metadata.write_profile,
        auto_increment_owner_boundary=auto_increment_metadata.owner_boundary,
        auto_increment_hotspot_risk=auto_increment_metadata.hotspot_risk,
        auto_increment_guidance=auto_increment_metadata.guidance,
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
    audited_auto_increment_tables = sorted(
        table for table in auto_increment_tables if table in AUTO_INCREMENT_METADATA
    )
    foreign_key_tables = sorted(table.table for table in tables if table.has_foreign_key)
    by_domain: dict[str, int] = {}
    for table in tables:
        by_domain[table.domain] = by_domain.get(table.domain, 0) + 1
    classified_tables = sorted(
        table.table for table in tables if table.table in TABLE_METADATA
    )

    return {
        "schema_sources": [asdict(source) for source in SCHEMA_SOURCES],
        "summary": {
            "source_count": len(SCHEMA_SOURCES),
            "table_declaration_count": len(tables),
            "unique_table_count": len(by_name),
            "duplicate_table_names": duplicates,
            "foreign_key_tables": foreign_key_tables,
            "auto_increment_tables": auto_increment_tables,
            "audited_auto_increment_tables": audited_auto_increment_tables,
            "unaudited_auto_increment_tables": sorted(
                set(auto_increment_tables) - set(audited_auto_increment_tables)
            ),
            "domain_table_counts": dict(sorted(by_domain.items())),
            "classified_table_count": len(classified_tables),
            "classified_tables": classified_tables,
            "unclassified_table_count": len(tables) - len(classified_tables),
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
        f"- Audited AUTO_INCREMENT tables: {len(summary['audited_auto_increment_tables'])}",
        f"- Classified tables: {summary['classified_table_count']}",
        f"- Unclassified tables: {summary['unclassified_table_count']}",
        "",
        "## Tables",
        "",
        "| Table | Domain | State class | Product owner | Primary query | Rebuildability | Merge guidance | AUTO_INCREMENT risk | AUTO_INCREMENT guidance | Source | Columns | Nullable | AUTO_INCREMENT | PK | Indexes |",
        "|---|---|---|---|---|---|---|---|---|---|---:|---:|---|---|---:|",
    ]
    for table in inventory["tables"]:
        assert isinstance(table, dict)
        auto_inc = ", ".join(table["auto_increment_columns"]) or "-"
        pk = ", ".join(table["primary_key"]) or "-"
        lines.append(
            "| `{table}` | `{domain}` | {state_class} | {product_owner} | {primary_query} | "
            "{rebuildability} | {merge_guidance} | {auto_increment_hotspot_risk} | "
            "{auto_increment_guidance} | `{source_path}:{source_line}` | "
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
