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
    "agent_sessions": TableMetadata(
        semantic_owner="astra_services::storage / session lifecycle",
        state_class="durable session lifecycle aggregate",
        primary_query="session ownership and list lookup by user_id, session_id, status, updated_at, last_active_at, and project_id",
        retention_policy="retain as the parent lifecycle row for transcript, events, runs, prompts, and project-scoped cleanup; session hard delete prunes owner/session children in ordered bounded batches before removing the parent row",
        rebuildability="not rebuildable as the authoritative session lifecycle aggregate because title, status, summary state, counters, project policy, and active plan/config pointers are product state",
        merge_guidance="do not merge into transcript, conversation_log, or agent_runs; this is the session aggregate and cleanup boundary",
        migration_owner="astra_services::storage",
        product_owner="session list, lifecycle cleanup, summaries, project retention",
    ),
    "agent_runs": TableMetadata(
        semantic_owner="astra_services::runs::DatabaseRunStateStore",
        state_class="durable run lifecycle authority",
        primary_query="run ownership, status, owner lease recovery, session active lists, retry lineage, and cursor pagination by user_id, run_id, session_id, status, updated_at, owner_pod_id, and owner_lease_expires_at",
        retention_policy="retain while run replay, recovery, pause/cancel, billing counters, retry lineage, and user-visible run history are needed; session hard delete prunes owner/session run rows after dependent run events, checkpoints, projections, prompts, and tool outputs are reconciled",
        rebuildability="not rebuildable from events without losing terminal status, owner lease generation, checkpoint pointer, counters, error state, runtime profile, and admission/recovery authority",
        merge_guidance="do not merge into agent_run_events or run_display_projections; this table owns mutable run authority while events own replay and projections own repairable display state",
        migration_owner="astra_services::runs",
        product_owner="runtime recovery, run lifecycle, run lists, billing counters",
    ),
    "run_checkpoints": TableMetadata(
        semantic_owner="astra_services::runs::DatabaseRunStateStore",
        state_class="durable run checkpoint fact",
        primary_query="latest and idempotent checkpoint lookup by user_id, run_id, checkpoint_kind, idempotency_key, node_seq, created_at, and session_id",
        retention_policy="no independent age-based TTL: retain while recovery, resume, retry, replay, or projection repair may need typed checkpoint payloads; session hard delete prunes owner/session checkpoints in ordered bounded batches after checkpoint consumers are inactive",
        rebuildability="not rebuildable once checkpoint_json is removed because it may contain compacted runtime state that is intentionally not present in event logs",
        merge_guidance="do not merge into agent_runs; checkpoints are typed, idempotent, multi-version payload facts with different cardinality and retention pressure",
        migration_owner="astra_services::runs",
        product_owner="run recovery, resume, retry, projection repair",
    ),
    "run_display_projections": TableMetadata(
        semantic_owner="astra_services::runs::DatabaseRunStateStore",
        state_class="derived run display projection",
        primary_query="hot run list and display hydration by user_id, run_id, session_id, updated_at, projection_event_idx, and latest checkpoint metadata",
        retention_policy="retain as a hot projection while run lists and display hydration need low-latency reads; repair jobs may clear and rebuild it from agent_runs, agent_run_events, and latest run_checkpoints",
        rebuildability="rebuildable from agent_runs, agent_run_events, and latest run_checkpoints when repair tooling is available",
        merge_guidance="do not merge into agent_runs; this is intentionally repairable display state rather than run lifecycle authority",
        migration_owner="astra_services::runs",
        product_owner="run list UI, display hydration, projection repair",
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
    "config_versions": TableMetadata(
        semantic_owner="astra_services::config_version_cloud / event_ingestion",
        state_class="durable tenant config version fact",
        primary_query="config version fetch by user_id/version_id and recent version list by user_id/created_at",
        retention_policy="retain while cloud config sync, first_seen_session linkage, and rollback/history need the TOML body; session hard delete clears first_seen_session without deleting the version fact",
        rebuildability="not rebuildable after toml_body is removed unless the same local config version still exists outside the database",
        merge_guidance="keep separate from agent_events; event ingestion may dual-write discovery events, but config_versions owns idempotent version fetch by user_id/version_id",
        migration_owner="astra_services::config_version_cloud",
        product_owner="cloud config sync and config rollback/history",
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
    "context_manifests": TableMetadata(
        semantic_owner="astra_services::context_manifest::ContextManifestStore",
        state_class="durable context manifest parent fact",
        primary_query="manifest lookup and history by user_id, session_id, run_id, created_at, and manifest_id",
        retention_policy="retain while context assembly diagnostics, manifest replay, and child context_manifest_items are needed; session deletion removes owner/session manifests after child items",
        rebuildability="rebuildable only by replaying the exact context assembly inputs, ranking decisions, token budgets, and raw refs",
        merge_guidance="keep separate from context_manifest_items; parent manifest summary and item fanout have different row cardinality and query paths",
        migration_owner="astra_services::storage / context_manifest",
        product_owner="LLM context assembly traceability",
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
    "session_state_revisions": TableMetadata(
        semantic_owner="astra_services::state_projection",
        state_class="durable session state revision watermark",
        primary_query="current revision and projection hash by user_id/session_id; updated sessions by user_id/updated_at",
        retention_policy="retain with the session while device sync, state projection integrity checks, and high-watermark reconciliation need monotonic revision state; session hard delete removes owner/session row",
        rebuildability="not safely rebuildable once monotonic_id, transcript/run high-watermarks, and device fingerprint lineage are lost",
        merge_guidance="keep separate from session_state_items; this is the session-level revision/watermark authority, not individual projected state",
        migration_owner="astra_services::storage / state_projection",
        product_owner="session state sync and projection integrity",
    ),
    "session_device_leases": TableMetadata(
        semantic_owner="runtime::server::session_handlers",
        state_class="durable device lease current state",
        primary_query="active lease lookup by user_id/session_id/device_id and expiry sweep by status/expires_at",
        retention_policy="retain current and recently revoked lease rows while device trust, session handoff, and expiry sweeps need lease state; session hard delete removes owner/session rows after lease events",
        rebuildability="not rebuildable after lease id, device fingerprint, trust level, and last_monotonic_id are lost",
        merge_guidance="keep separate from session_device_lease_events; lease table is mutable current state while events are immutable audit",
        migration_owner="astra_services::storage / runtime session handlers",
        product_owner="session device handoff and trust controls",
    ),
    "session_device_lease_events": TableMetadata(
        semantic_owner="runtime::server::session_handlers / device_lease_sweeper",
        state_class="durable device lease audit event",
        primary_query="lease audit by user_id/session_id/device_id/created_at and expiry/revoke diagnostics by event_type/created_at",
        retention_policy="retain with session_device_leases while device trust audits and expiry diagnostics are needed; session hard delete removes owner/session events in ordered batches",
        rebuildability="not rebuildable after revoke/expiry reasons and ended_at_server audit facts are lost",
        merge_guidance="keep separate from session_device_leases; event history has append-only audit semantics and different retention pressure",
        migration_owner="astra_services::storage / runtime session handlers",
        product_owner="session device lease audit and sweeper diagnostics",
    ),
    "session_state_items": TableMetadata(
        semantic_owner="astra_services::state_projection",
        state_class="durable state projection current item",
        primary_query="current state lookup by user_id/session_id/scope/category/item_key and cross-session/user scoped state by user_id/scope/category/status/priority",
        retention_policy="retain while session/user/project/workspace state projection is active; expires_at and session hard delete prune rows only after audit/revision invariants are preserved",
        rebuildability="partially rebuildable only while source events, payloads, and projection rules remain complete",
        merge_guidance="keep separate from session_state_item_events; this table is the current projection surface while events preserve mutation history",
        migration_owner="astra_services::storage / state_projection",
        product_owner="session memory/state projection and active task/delegation context",
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
    "session_delegations": TableMetadata(
        semantic_owner="astra_services::state_projection / runtime delegation engine",
        state_class="durable delegation topology fact",
        primary_query="delegation tree lookup by user_id/root_run_id/depth, parent status by user_id/parent_run_id/status, and session status by user_id/session_id/status",
        retention_policy="retain while multi-agent tree display, retry lineage, sibling artifact exposure, and recovery need parent/child topology; session hard delete removes owner/session rows after child runs are reconciled",
        rebuildability="not fully rebuildable from agent_runs without losing ancestor_path, retry_scope, sibling exposure metadata, and delegation-specific summaries",
        merge_guidance="do not merge into agent_runs; run lifecycle authority and delegation topology have different mutation owners and query shapes",
        migration_owner="astra_services::storage / state_projection",
        product_owner="multi-agent delegation tree, retries, and progress display",
    ),
    "session_plan_todos": TableMetadata(
        semantic_owner="astra_services::state_projection",
        state_class="durable plan/backlog todo projection",
        primary_query="active plan todo lookup by user_id/session_id/status/priority and backlog pool lookup by user_id/backlog_pool_id/status",
        retention_policy="retain while plan/backlog projection and state sync need plan todo hierarchy; supersede marks old rows inactive and session hard delete removes owner/session rows",
        rebuildability="rebuildable only while source plan state and todo seed inputs remain available",
        merge_guidance="do not merge with session_todos; this is plan/backlog projection state with incompatible schema and consumers",
        migration_owner="astra_services::storage / state_projection",
        product_owner="plan-driven todo projection and backlog pools",
    ),
    "session_todos": TableMetadata(
        semantic_owner="runtime::server::session_todo_handlers / astra_tools::task_mgmt",
        state_class="durable live task board fact",
        primary_query="task board hydration by user_id/session_id/ordinal, status updates by user_id/session_id/status/updated_at, and user task lists by user_id/status/updated_at",
        retention_policy="retain as the authoritative session task board until task archive/GC or session hard delete; archived rows may be pruned only after dependency cleanup and idempotency/counter invariants remain intact",
        rebuildability="not rebuildable after task ids, ordinals, dependency metadata, subtasks, and user-visible edits are lost",
        merge_guidance="do not merge with session_plan_todos; session_todos is the live task scratchpad, not the plan/backlog projection",
        migration_owner="astra_services::storage / runtime session_todo_handlers",
        product_owner="task tool, session task board, TUI/dashboard task surface",
    ),
    "session_todo_counters": TableMetadata(
        semantic_owner="runtime::server::session_todo_handlers",
        state_class="durable monotonic task id allocator",
        primary_query="owner/session next task id lookup and FOR UPDATE increment by user_id/session_id",
        retention_policy="retain while the session task board may create new task ids; session hard delete removes owner/session counter after todos are gone",
        rebuildability="not safely rebuildable from existing todos because deleted task ids must not be reused",
        merge_guidance="do not merge into session_todos; deleted todos still reserve ids, so the allocator has a different lifecycle from task rows",
        migration_owner="astra_services::storage / runtime session_todo_handlers",
        product_owner="task tool id stability and idempotent task creation",
    ),
    "session_todo_idempotency": TableMetadata(
        semantic_owner="runtime::server::session_todo_handlers",
        state_class="durable task action idempotency fact",
        primary_query="task action replay lookup by user_id/session_id/action/idempotency_key",
        retention_policy="retain at least while task action retries, client reconnects, and session replay can repeat the same idempotency key; session hard delete removes owner/session rows",
        rebuildability="not rebuildable after args_json/output are lost because duplicate retries would re-execute side effects",
        merge_guidance="keep separate from session_todos; idempotency keys are per action, can outlive a specific task row, and must be queried directly",
        migration_owner="astra_services::storage / runtime session_todo_handlers",
        product_owner="task tool retry safety and exactly-once action behavior",
    ),
    "data_versioning_checkpoints": TableMetadata(
        semantic_owner="astra_services::data_versioning::DatabaseDataVersioningService",
        state_class="durable named data checkpoint fact",
        primary_query="checkpoint lookup by checkpoint_id and user-scoped checkpoint_name",
        retention_policy="retain while data versioning rollback/list APIs need named checkpoints; delete only through data versioning lifecycle, not generic schema cleanup",
        rebuildability="not rebuildable after checkpoint identity/name and created_at audit timestamp are lost",
        merge_guidance="do not delete as dead DDL; DatabaseDataVersioningService reads and writes this table",
        migration_owner="astra_services::storage / data_versioning",
        product_owner="data versioning checkpoints and rollback/list workflows",
    ),
    "sweeper_leases": TableMetadata(
        semantic_owner="runtime::server::sweeper_lease",
        state_class="coordination leader lease",
        primary_query="sweeper ownership CAS by sweeper_name, owner_pod_id, expires_at, and version",
        retention_policy="retain one row per sweeper type while the background job exists; expired rows are reused by CAS rather than age-pruned as audit",
        rebuildability="rebuildable by recreating missing lease rows, but current ownership is live coordination state",
        merge_guidance="keep separate from job-specific tables; this is shared multi-pod leader election coordination",
        migration_owner="astra_services::storage / runtime sweeper_lease",
        product_owner="background cleanup and multi-pod duplicate-work prevention",
    ),
    "workspace_records": TableMetadata(
        semantic_owner="astra_services::workspace_records::DatabaseWorkspaceRecordStore",
        state_class="durable workspace lifecycle fact",
        primary_query="workspace lookup/list by workspace_id, owner_id, source_key, and updated_at",
        retention_policy="retain while a server/cloud workspace can be reused, cleaned, or audited; session deletion enqueues cleanup debts before deleting owner/session records",
        rebuildability="not rebuildable after root_or_volume_ref, source_json, revision, and record_json are lost",
        merge_guidance="keep separate from agent_runs/session_artifacts; workspace lifecycle and cleanup authority differ from run history and artifact refs",
        migration_owner="astra_services::workspace_records",
        product_owner="workspace provisioning, cleanup, and reusable workspace inventory",
    ),
    "workspace_cleanup_debts": TableMetadata(
        semantic_owner="astra_services::workspace_records::DatabaseWorkspaceRecordStore",
        state_class="durable workspace cleanup debt",
        primary_query="pending cleanup debt claim/list by owner_id, resolved_at, attempts, and updated_at",
        retention_policy="retain unresolved debts until cleanup succeeds; resolved debts may be pruned after operational retention, but not before workspace cleanup evidence is no longer needed",
        rebuildability="not rebuildable after source workspace record is deleted unless the debt row preserves record_json",
        merge_guidance="keep separate from workspace_records; cleanup debt can outlive the workspace record and has retry/attempt lifecycle",
        migration_owner="astra_services::workspace_records",
        product_owner="workspace cleanup reliability and operational recovery",
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
