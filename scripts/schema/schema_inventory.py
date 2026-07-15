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
        path="crates/services/src/storage.rs",
        startup_owner="ensure_core_schema",
        state_class_hint="mixed",
        hot_path_hint="mixed",
    ),
    SchemaSource(
        owner="astra_services::config_version_cloud",
        domain="config_versions",
        path="crates/services/src/config_version_cloud.rs",
        startup_owner="ensure_core_schema via CONFIG_VERSIONS_CREATE_SQL",
        state_class_hint="durable fact",
        hot_path_hint="warm append/read",
    ),
    SchemaSource(
        owner="astra_messaging::db_transport",
        domain="messaging",
        path="crates/astra-messaging/src/db_transport.rs",
        startup_owner="astra_messaging::db_transport::ensure_schema",
        state_class_hint="coordination fact",
        hot_path_hint="hot queue",
    ),
    SchemaSource(
        owner="astra_services::resource_governor",
        domain="resource_governor",
        path="crates/services/src/resource_governor.rs",
        startup_owner="DatabaseResourceGovernor::ensure_tables",
        state_class_hint="quota fact",
        hot_path_hint="warm quota read/write",
    ),
    SchemaSource(
        owner="astra_services::workspace_records",
        domain="workspace_records",
        path="crates/services/src/workspace_records.rs",
        startup_owner="DatabaseWorkspaceRecordStore::ensure_tables",
        state_class_hint="durable workspace fact / cleanup debt",
        hot_path_hint="run start/end workspace persistence",
    ),
    SchemaSource(
        owner="astra_runtime::llm_provider_admission",
        domain="runtime_admission",
        path="crates/runtime/src/llm_provider_admission.rs",
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
class ConsolidationReview:
    candidate: str
    decision: str
    current_read_paths: list[str]
    current_write_paths: list[str]
    user_api_impact: str
    migration_backfill: str
    rollback: str
    test_evidence: list[str]
    rationale: str


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
    "agent_event_edges": TableMetadata(
        semantic_owner="astra_services::storage::insert_agent_event_edges",
        state_class="durable event lineage edge fact",
        primary_query="event DAG lineage by user_id, session_id, child_event_id, parent_event_id, relation_kind, and parent_order",
        retention_policy="retain with agent_events while causal lineage, replay diagnostics, and graph queries need parent/child ordering; session hard delete and event cleanup prune edges before deleting parent agent_events",
        rebuildability="not rebuildable after parent_event_ids and ordered relation_kind context are dropped from ingestion/replay inputs",
        merge_guidance="keep separate from agent_events; edges can fan out per child event and have different indexes, but must be deleted before event rows",
        migration_owner="astra_services::storage",
        product_owner="event lineage, causal graph introspection, replay diagnostics",
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
    "session_artifacts": TableMetadata(
        semantic_owner="astra_services::session_artifact_store / artifact_retention_sweeper",
        state_class="durable session artifact fact",
        primary_query="artifact lookup/list by user_id, session_id, artifact_id, artifact_kind, source, owner_run_id, root_run_id, access_scope, status, and retention_until",
        retention_policy="retain while artifact previews, manifest refs, state items, citations, grants, and project retention need the content_json; retention sweeper marks expiring/expired and session hard delete removes owner/session rows after grants",
        rebuildability="not rebuildable after content_json, metadata, cold_storage_ref, derived_from_artifact_id, and reference counters are lost",
        merge_guidance="keep separate from session_artifacts_grants; artifacts own content and retention state while grants own cross-run/delegation visibility",
        migration_owner="astra_services::storage / session_artifact_store",
        product_owner="work surface artifacts, previews, retention, delegation visibility",
    ),
    "session_artifacts_grants": TableMetadata(
        semantic_owner="astra_services::state_projection / artifact grants",
        state_class="durable artifact visibility grant fact",
        primary_query="artifact grant lookup by user_id, session_id, artifact_id, grant_scope, target_run_id, target_delegation_id, root_run_id, and expires_at",
        retention_policy="retain while delegated runs or sibling tasks may access granted artifacts; session hard delete removes grants before artifact rows",
        rebuildability="not rebuildable after grant_scope, target_run_id/delegation_id, reason, and expires_at are lost",
        merge_guidance="keep separate from session_artifacts; grants are visibility/control-plane facts with many targets per artifact",
        migration_owner="astra_services::storage / state_projection",
        product_owner="artifact sharing across delegation tree and work surface permissions",
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
    "tool_invocation_ledger": TableMetadata(
        semantic_owner="astra_services::tool_invocation_ledger",
        state_class="durable invocation coordination fact",
        primary_query="atomic invocation prepare and state transition by full owner/session/run/turn/invocation identity",
        retention_policy="retain for the complete replay and reconciliation lifetime of the owning session/run; session hard delete removes owner/session rows",
        rebuildability="not rebuildable safely after dispatch; it is the authority preventing duplicate delivery and preserving outcome uncertainty",
        merge_guidance="keep separate from semantic result caches and event logs; invocation CAS and reconciliation have different identity and lifecycle contracts",
        migration_owner="astra_services::storage / tool_invocation_ledger",
        product_owner="tool invocation durability, retry, reconnect, and resume safety",
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
    "infra_llm_models": TableMetadata(
        semantic_owner="astra_services::models",
        state_class="durable model registry and credential fact",
        primary_query="active/preferred model resolution by model_name, provider, and is_active; admin model list/update/delete by model_id and model_name",
        retention_policy="retain while a model can be selected for runtime LLM calls; delete/deactivate only through model admin APIs so active-model cache invalidation and encrypted credential lifecycle remain coherent",
        rebuildability="not rebuildable after encrypted API key, provider endpoint, pricing, quirks, tags, and thinking probe state are lost",
        merge_guidance="do not merge into admin_config; this is the structured model registry, while admin_config stores small named overrides such as reasoning_model_name",
        migration_owner="astra_services::storage / models",
        product_owner="model registry, model resolution, LLM credential administration",
    ),
    "model_gateways": TableMetadata(
        semantic_owner="astra_services::model_gateways",
        state_class="durable external model gateway registry fact",
        primary_query="gateway resolve endpoint lookup by id and active gateway listing by status/created_at",
        retention_policy="retain while capability descriptors or selected_model_gateway references may resolve through the gateway; disable instead of deleting when runs may still reference the gateway id",
        rebuildability="rebuildable only from external gateway registration/configuration if it still exists",
        merge_guidance="keep separate from infra_llm_models; gateways resolve external runtime capabilities and have endpoint/protocol lifecycle distinct from concrete model credentials",
        migration_owner="astra_services::storage / model_gateways",
        product_owner="external runtime model gateway and capability descriptors",
    ),
    "runtime_llm_trusted_domains": TableMetadata(
        semantic_owner="astra_services::llm_trusted_domains",
        state_class="durable LLM endpoint trust policy fact",
        primary_query="trusted endpoint allowlist lookup/list by domain_host, domain_port, is_enabled, and domain_id",
        retention_policy="retain enabled/disabled policy rows while runtime endpoint validation and admin audit need the trust decision; delete only through trusted-domain admin API",
        rebuildability="rebuildable only from a separate security policy source if one exists",
        merge_guidance="keep separate from infra_llm_models and model_gateways; this table owns host/port trust policy, not model credentials or gateway resolution",
        migration_owner="astra_services::storage / llm_trusted_domains",
        product_owner="LLM endpoint security policy and admin controls",
    ),
    "skills_registry": TableMetadata(
        semantic_owner="astra_services::skills / marketplace",
        state_class="durable shared skill catalog fact",
        primary_query="visible skill list/get by is_active, is_public, created_by, skill_name, version, skill_id, source, and status",
        retention_policy="retain while web/runtime skill resolution, marketplace search, installation, and skill info APIs may reference the skill; unpublish/deprecate toggles status/is_active instead of deleting public history",
        rebuildability="not rebuildable after skill_definition, manifest, publisher metadata, trust tier, and source ownership are lost",
        merge_guidance="keep separate from user_skill_sources/user_skill_versions; skills_registry is the shared runtime catalog while personal skill tables own authoring history",
        migration_owner="astra_services::storage / skills",
        product_owner="web/runtime skill catalog, marketplace, skill resolution",
    ),
    "skill_metrics": TableMetadata(
        semantic_owner="astra_services::marketplace_stats",
        state_class="derived marketplace metric projection",
        primary_query="skill stats and ranking lookup by skill_name, metric_type, metric_slot, avg_quality, active_users_7d, trust_tier, and updated_at",
        retention_policy="retain aggregate rows while marketplace ranking and stats APIs need them; raw quality report rows can be compacted only after aggregate refresh preserves required counters",
        rebuildability="partially rebuildable from installs, quality reports, evaluation events, and feedback only while those source facts remain complete",
        merge_guidance="keep separate from skills_registry; metrics are high-churn projections with ranking indexes and can be refreshed independently of catalog metadata",
        migration_owner="astra_services::storage / marketplace_stats",
        product_owner="skill marketplace ranking, trust, quality stats",
    ),
    "skill_selection_events": TableMetadata(
        semantic_owner="runtime data_layer / evaluation skill audit",
        state_class="durable skill selection audit event",
        primary_query="session/user skill selection audit by user_id, session_id, created_at and skill_name/created_at",
        retention_policy="retain with the session while evaluation, self-awareness scenarios, and skill selection audit need selection and execution metrics; session hard delete removes owner/session rows",
        rebuildability="not fully rebuildable after query, selected_skills, execution_success, and feedback score are dropped",
        merge_guidance="do not merge into agent_events until skill selection audit/evaluation readers stop querying this table directly",
        migration_owner="astra_services::storage / runtime data_layer",
        product_owner="skill selection audit, evaluation, self-awareness diagnostics",
    ),
    "skill_installations": TableMetadata(
        semantic_owner="astra_services::marketplace / personal_skills",
        state_class="durable user skill installation fact",
        primary_query="installed skill lookup/list by user_id, skill_name, installation_id, status, installed_at, scope, session_id, workspace_id, and auto_activate_on_topic_match",
        retention_policy="retain while the user/session/workspace skill installation is active or rollback needs previous_version; session hard delete removes session-scoped installations",
        rebuildability="not rebuildable after install status, previous_version, scope, and auto-activation preference are lost",
        merge_guidance="keep separate from skills_registry; catalog availability and a user's installed/activated state have different lifecycles",
        migration_owner="astra_services::storage / marketplace",
        product_owner="skill marketplace install state and auto-activation",
    ),
    "skill_settings": TableMetadata(
        semantic_owner="astra_services::skill_config",
        state_class="durable skill configuration fact",
        primary_query="effective config lookup by skill_name, setting_name, scope_type, scope_id, and skill-level validation",
        retention_policy="retain while the skill and scope need configuration; delete only through skill config APIs so secret masking and effective-config precedence remain coherent",
        rebuildability="not rebuildable after setting values, scope, and secret flags are lost",
        merge_guidance="keep separate from skill_user_credentials and skill_resource_bindings; settings, credentials, and resource bindings have different secrecy and lookup semantics",
        migration_owner="astra_services::storage / skill_config",
        product_owner="skill runtime configuration and validation",
    ),
    "skill_resource_bindings": TableMetadata(
        semantic_owner="astra_services::skill_config",
        state_class="durable skill resource binding fact",
        primary_query="resource binding lookup by user_id, skill_name, resource_type, resource_key, and binding_name",
        retention_policy="retain while a user skill can access the bound resource; delete only through skill resource binding APIs",
        rebuildability="not rebuildable after binding_value, resource_key, and secret flag are lost",
        merge_guidance="keep separate from skill_settings; resource bindings are per user/resource and may reference secrets or external resources",
        migration_owner="astra_services::storage / skill_config",
        product_owner="skill resource access and runtime validation",
    ),
    "skill_user_credentials": TableMetadata(
        semantic_owner="astra_services::marketplace",
        state_class="durable encrypted skill credential fact",
        primary_query="credential save/delete by user_id, skill_name, credential_name and credential_id",
        retention_policy="retain encrypted credentials until the user/admin deletes them; never prune as derived marketplace metadata",
        rebuildability="not rebuildable after value_encrypted is lost",
        merge_guidance="keep separate from skill_settings and skill_resource_bindings; credentials are encrypted user secrets with a narrower trust boundary",
        migration_owner="astra_services::storage / marketplace",
        product_owner="per-user skill credentials and secret management",
    ),
    "user_skill_sources": TableMetadata(
        semantic_owner="astra_services::personal_skills",
        state_class="durable personal skill source fact",
        primary_query="owner skill source lookup/list by owner_user_id, skill_name, visibility, status, and updated_at",
        retention_policy="retain while a user's authored skill exists; archive via status/visibility rather than deleting versions and evaluations prematurely",
        rebuildability="not rebuildable after source_id, ownership, visibility, and lifecycle state are lost",
        merge_guidance="keep separate from user_skill_versions; source owns authoring identity while versions own content snapshots",
        migration_owner="astra_services::storage / personal_skills",
        product_owner="personal skill authoring and publishing workflow",
    ),
    "user_skill_versions": TableMetadata(
        semantic_owner="astra_services::personal_skills",
        state_class="durable personal skill version content fact",
        primary_query="owner skill versions by owner_user_id, skill_name, source_id, version_id, status, content_hash, and created_at",
        retention_policy="retain while drafts, published versions, superseded history, evaluation, or rollback need the manifest/content snapshot",
        rebuildability="not rebuildable after manifest_json, content_markdown, content_hash, and normalization version are lost",
        merge_guidance="keep separate from user_skill_sources and skills_registry; this is authoring/version content, not shared runtime catalog state",
        migration_owner="astra_services::storage / personal_skills",
        product_owner="personal skill authoring, review, publish, rollback",
    ),
    "user_skill_evaluations": TableMetadata(
        semantic_owner="astra_services::personal_skills",
        state_class="durable personal skill evaluation fact",
        primary_query="evaluation lookup by owner_user_id, evaluation_id, source_id, version_id, and run_id",
        retention_policy="retain while skill quality review, publish decisions, and run-linked audit need evaluation payloads; session hard delete removes rows linked to agent_runs for the deleted session",
        rebuildability="not fully rebuildable after payload_json, hit/suspect counts, false positives, and run linkage are dropped",
        merge_guidance="keep separate from user_skill_versions; evaluations are run-linked review facts with different retention and query paths",
        migration_owner="astra_services::storage / personal_skills",
        product_owner="personal skill quality review and publishing decisions",
    ),
    "edge_pending_dispatch": TableMetadata(
        semantic_owner="astra_services::multi_agent::edge_dispatch",
        state_class="coordination dispatch fact",
        primary_query="turn-scoped dispatch lookup by user_id, session_id, run_id, turn_chain_id, and request_id; edge poll by user_id, edge_agent_id, status, created_at",
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
    "session_todos": TableMetadata(
        semantic_owner="runtime::server::session_todo_handlers / astra_tools::task_mgmt",
        state_class="durable live task board fact",
        primary_query="task board hydration by user_id/session_id/ordinal, status updates by user_id/session_id/status/updated_at, and user task lists by user_id/status/updated_at",
        retention_policy="retain as the authoritative session task board until task archive/GC or session hard delete; archived rows may be pruned only after dependency cleanup and idempotency/counter invariants remain intact",
        rebuildability="not rebuildable after task ids, ordinals, dependency metadata, subtasks, and user-visible edits are lost",
        merge_guidance="do not use this table as a plan mirror; durable plan and step runs are projected read-only, while session_todos owns user and task-tool checklist state",
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
    "session_checkpoints": TableMetadata(
        semantic_owner="astra_services::session_restore",
        state_class="durable session restore checkpoint fact",
        primary_query="checkpoint lookup by user_id, session_id, checkpoint_id, number, turn, and created_at",
        retention_policy="retain while session restore, rollback, tool state, contract state, and checkpoint list APIs may need state_json/tools_json; session hard delete removes owner/session rows",
        rebuildability="not rebuildable after state_json, tools_json, contract_state_json, summary, and token/error counters are lost",
        merge_guidance="keep separate from run_checkpoints; this table owns session-level restore snapshots while run_checkpoints own typed run recovery payloads",
        migration_owner="astra_services::storage / session_restore",
        product_owner="session restore, checkpoint list, rollback",
    ),
    "user_preferences": TableMetadata(
        semantic_owner="astra_services::state_sync",
        state_class="durable user preference fact",
        primary_query="preference read/update by user_id, pref_key, version, and updated_at; pull sync by user_id",
        retention_policy="retain until the user or sync layer overwrites/deletes the preference; version increments are used for conflict detection",
        rebuildability="not rebuildable after pref_value and version are lost unless an external preference source exists",
        merge_guidance="keep separate from admin_config; user_preferences are per-user sync state while admin_config is server control-plane state",
        migration_owner="astra_services::storage / state_sync",
        product_owner="user preferences, edge/cloud sync, prompt personalization",
    ),
    "admin_config": TableMetadata(
        semantic_owner="astra_services::admin_config",
        state_class="durable server admin configuration fact",
        primary_query="configuration lookup and list by config_key, including reasoning_model_name model selection override",
        retention_policy="retain until the admin explicitly unsets the key; delete means fall back to code/default behavior",
        rebuildability="rebuildable only from external admin configuration or operator intent if recorded elsewhere",
        merge_guidance="keep separate from infra_llm_models; admin_config stores named control-plane overrides, not the model registry itself",
        migration_owner="astra_services::storage / admin_config",
        product_owner="admin runtime configuration and model selection controls",
    ),
    "auth_users": TableMetadata(
        semantic_owner="astra_services::auth",
        state_class="durable user identity fact",
        primary_query="login/current-user lookup by username, email, and user_id; last_login_at update by user_id",
        retention_policy="retain while the account exists; deactivate via is_active rather than deleting while roles, refresh tokens, sessions, audit logs, or external identities reference the user",
        rebuildability="not rebuildable after password hash, display name, activation state, and login identity are lost",
        merge_guidance="keep separate from auth_refresh_tokens, auth_user_roles, and external identities; identity, grants, and sessions have different lifecycles",
        migration_owner="astra_services::storage / auth",
        product_owner="authentication, user account management, current-user APIs",
    ),
    "auth_roles": TableMetadata(
        semantic_owner="astra_services::auth",
        state_class="durable auth role definition fact",
        primary_query="role lookup by role_name and role_id during default-role bootstrap, grant, revoke, and admin member lists",
        retention_policy="retain while any role can be granted or queried; delete only after dependent auth_user_roles grants are removed",
        rebuildability="rebuildable only for built-in default roles; custom roles require an external admin source",
        merge_guidance="keep separate from auth_user_roles; role definitions and user grants have different cardinality and revoke semantics",
        migration_owner="astra_services::storage / auth",
        product_owner="auth and admin role management",
    ),
    "auth_refresh_tokens": TableMetadata(
        semantic_owner="astra_services::auth",
        state_class="durable refresh-session credential fact",
        primary_query="refresh/logout lookup by token_hash, session_id, user_id, is_revoked, and expires_at",
        retention_policy="cleanup_expired_data prunes expired or revoked rows after refresh_token_days default 7 in ordered bounded batches; logout and refresh revoke specific token hashes",
        rebuildability="not rebuildable after token_hash/session binding is lost because refresh/logout idempotency and external session linkage depend on it",
        merge_guidance="keep separate from auth_users; refresh credentials are high-churn secrets with TTL/revocation lifecycle",
        migration_owner="astra_services::storage / auth",
        product_owner="auth refresh and logout continuity",
    ),
    "auth_tokens": TableMetadata(
        semantic_owner="astra_services::auth::admin",
        state_class="durable admin-managed secret/token fact",
        primary_query="admin token list/filter by is_active, scope_user_id, scope_repo, provider, and token_id",
        retention_policy="cleanup_expired_data prunes inactive rows after auth_token_days default 30 in ordered bounded batches; active secrets must remain until admin revoke/delete",
        rebuildability="not rebuildable after encrypted_value or secret_ref is lost",
        merge_guidance="keep separate from auth_refresh_tokens; admin-managed provider/API secrets and user refresh sessions have different trust boundaries and TTL semantics",
        migration_owner="astra_services::storage / auth",
        product_owner="admin secret/token management and scoped provider credentials",
    ),
    "auth_audit_logs": TableMetadata(
        semantic_owner="astra_services::auth::admin / auth session audit",
        state_class="durable auth audit event",
        primary_query="audit list by user_id, created_at, log_id and resource lookup by user_id/resource_type/resource_id/created_at",
        retention_policy="cleanup_expired_data prunes old audit rows after audit_log_days default 90 in ordered bounded batches; retain within window for security and admin traceability",
        rebuildability="not rebuildable after request details, resource id, ip address, and event time are dropped",
        merge_guidance="keep separate from auth_users and tracing; this is a product/security audit table queried by auth session APIs",
        migration_owner="astra_services::storage / auth",
        product_owner="auth audit, admin traceability, security review",
    ),
    "auth_user_roles": TableMetadata(
        semantic_owner="astra_services::auth::admin / auth registration",
        state_class="durable auth grant fact",
        primary_query="role membership lookup and grant/revoke by user_id and role_id; admin role member lookup by role_id",
        retention_policy="retain while the user-role grant is active; delete is revoke",
        rebuildability="rebuildable only from the configured account authority if one exists",
        merge_guidance="keep separate from auth_users/auth_roles; this is the many-to-many grant fact with independent revoke lifecycle",
        migration_owner="astra_services::storage / auth",
        product_owner="auth and admin role management",
    ),
    "agent_agents": TableMetadata(
        semantic_owner="astra_services::agents",
        state_class="durable user agent definition fact",
        primary_query="agent lookup/list/update/delete by agent_id, owner_user_id, agent_name, agent_type, and is_active",
        retention_policy="retain while the owner can use or audit the agent definition; delete only through agent service after ownership checks",
        rebuildability="not rebuildable after agent_config, data_source, owner, and active state are lost",
        merge_guidance="keep separate from agent_bindings; agent_agents stores user-owned agent definitions while agent_bindings stores runtime binding descriptors",
        migration_owner="astra_services::storage / agents",
        product_owner="agent management and user-owned agent definitions",
    ),
    "agent_bindings": TableMetadata(
        semantic_owner="astra_services::agent_bindings",
        state_class="durable runtime agent binding descriptor",
        primary_query="binding lookup by id, binding_name, idempotency_key, status, and created_at",
        retention_policy="retain while runs, runtime profiles, or clients may reference the binding id/name; disable instead of deleting when historical runs can display binding metadata",
        rebuildability="not rebuildable after agent_md, capability_servers_json, runtime_policy_json, metadata_json, and binding_schema_version are lost",
        merge_guidance="keep separate from agent_agents and agent_runs; this table packages runtime capability binding descriptors with idempotent creation semantics",
        migration_owner="astra_services::storage / agent_bindings",
        product_owner="web agent binding, runtime capability descriptors, run creation UX",
    ),
    "agent_tasks": TableMetadata(
        semantic_owner="astra_services::task_orchestrator / multi_agent::task_lease",
        state_class="durable task orchestration fact",
        primary_query="task detail/list/search by user_id, task_id, status, session_id, updated_at, title, and task lease joins through agent_id",
        retention_policy="retain while task board, long-running task recovery, checkpoint, feedback, and task lease coordination need task state; session hard delete removes owner/session tasks after dependent task_leases",
        rebuildability="not rebuildable after plan_json, checkpoint_json, progress, feedback, outcome, and worker agent_id state are lost",
        merge_guidance="keep separate from session_todos and task_leases; agent_tasks owns durable orchestration state, todos own user scratchpad tasks, leases own worker coordination",
        migration_owner="astra_services::storage / task_orchestrator",
        product_owner="long task orchestration, task board, worker lease coordination",
    ),
    "harness_snapshots": TableMetadata(
        semantic_owner="astra_services::harness diagnostics",
        state_class="durable harness diagnostic snapshot",
        primary_query="harness diagnostic snapshot lookup by user_id, session_id, created_at, turn_number, and causal_chain_id",
        retention_policy="retain with session diagnostic history while harness replay, hook debugging, and causal-chain inspection need snapshot_json; session hard delete removes owner/session rows",
        rebuildability="not rebuildable after snapshot_json and causal_chain_id are dropped from the hook execution path",
        merge_guidance="keep separate from agent_events; harness snapshots intentionally avoid polluting session event counts and carry hook_point/turn_number payloads with different query pressure",
        migration_owner="astra_services::storage / harness diagnostics",
        product_owner="harness replay, hook diagnostics, causal-chain debugging",
    ),
    "harness_runs": TableMetadata(
        semantic_owner="astra_services::harness product workflow",
        state_class="durable harness workflow parent fact",
        primary_query="harness run lookup/list by harness_run_id, harness_id, user_id, status, session_id, and updated_at",
        retention_policy="retain while generated items, skill drafts, rules, citations, and user workflow history may reference the run; delete children before the parent during workflow cleanup",
        rebuildability="not rebuildable after input_json, output_json, status, error, and run linkage are lost",
        merge_guidance="keep separate from harness_items and harness_snapshots; runs are product workflow parents while items fan out and snapshots are diagnostics",
        migration_owner="astra_services::storage / harness workflow",
        product_owner="Skillify and reusable harness workflow state",
    ),
    "harness_items": TableMetadata(
        semantic_owner="astra_services::harness product workflow",
        state_class="durable harness item fact",
        primary_query="harness item queue/list by harness_run_id, status, updated_at, parent_item_id, item_type, and assigned_to",
        retention_policy="retain with parent harness_runs while review decisions, final outputs, and assignment state are visible or auditable; workflow cleanup removes items before the run",
        rebuildability="not rebuildable after locator_json, proposed_output_json, final_output_json, decision history, and assignment state are lost",
        merge_guidance="keep separate from harness_runs; items are per-candidate/review facts with higher cardinality and independent decision state",
        migration_owner="astra_services::storage / harness workflow",
        product_owner="harness review queue, generated item decisions, workflow audit",
    ),
    "harness_skill_drafts": TableMetadata(
        semantic_owner="astra_services::harness skill generation",
        state_class="durable generated skill draft fact",
        primary_query="skill draft lookup/list by skill_draft_id, harness_run_id, status, revision, and candidate_name",
        retention_policy="retain while generated skill drafts can be reviewed, revised, published, or audited; cleanup must remove dependent harness_skill_rules and harness_citations first",
        rebuildability="not rebuildable after content_markdown, source_summary_json, decision history, revision, and published_version_id are lost",
        merge_guidance="keep separate from harness_items and user_skill_versions; drafts are pre-publication generated skill candidates with review/publish lifecycle",
        migration_owner="astra_services::storage / harness skill generation",
        product_owner="Skillify draft review and publish workflow",
    ),
    "harness_skill_rules": TableMetadata(
        semantic_owner="astra_services::harness skill generation",
        state_class="durable generated skill rule fact",
        primary_query="skill rule lookup/list by skill_draft_id, harness_run_id, status, rule_type, and updated_at",
        retention_policy="retain with parent skill drafts while review, citation evidence, and publish decisions need rule statements and rationale; cleanup removes rules before drafts",
        rebuildability="not rebuildable after statement, rationale, decision history, source_count, and created_by_node_id are lost",
        merge_guidance="keep separate from harness_skill_drafts; rules fan out from a draft and carry separately reviewable evidence-backed assertions",
        migration_owner="astra_services::storage / harness skill generation",
        product_owner="Skillify rule review, evidence mapping, publish decisions",
    ),
    "harness_citations": TableMetadata(
        semantic_owner="astra_services::harness evidence",
        state_class="durable harness citation/evidence fact",
        primary_query="citation lookup/list by harness_run_id, item_id, skill_rule_id, skill_draft_id, and created_at",
        retention_policy="retain with harness items/rules while evidence previews, source hashes, and auditability are required; cleanup removes citations before their referenced item, rule, draft, or run rows",
        rebuildability="not rebuildable after source_locator_json, source_snapshot_ref, quote_hash, evidence_text_preview, and relevance_score are lost",
        merge_guidance="keep separate from harness_items and harness_skill_rules; citations are evidence fanout rows and may later compact cold source metadata without merging the table",
        migration_owner="astra_services::storage / harness evidence",
        product_owner="harness evidence audit, source traceability, generated skill review",
    ),
    "ctx_snapshots": TableMetadata(
        semantic_owner="astra_services::context diagnostics",
        state_class="durable context assembly diagnostic snapshot",
        primary_query="context snapshot lookup by user_id, session_id, created_at and event_id",
        retention_policy="retain with session diagnostics while prompt/context debugging, token accounting, and decision audits need captured context; session hard delete removes owner/session rows",
        rebuildability="not rebuildable after context_data, llm_request_id, llm_response_id, token stats, relevance scores, and task_type are dropped",
        merge_guidance="keep separate from agent_events and context_manifests; ctx_snapshots store captured context payloads and token diagnostics, not timeline events or manifest item ordering",
        migration_owner="astra_services::storage / context diagnostics",
        product_owner="context assembly diagnostics, prompt debugging, evaluation traceability",
    ),
    "ctx_decision_audits": TableMetadata(
        semantic_owner="astra_services::context decision audit",
        state_class="durable context decision audit fact",
        primary_query="context decision audit by user_id, session_id, decision_type, created_at, event_id, and context_capture_id",
        retention_policy="retain with context snapshots while routing, assembly, or model-selection decisions need audit payloads; session hard delete removes owner/session rows",
        rebuildability="not rebuildable after decision_output, model_params, model_used, and context_capture linkage are lost",
        merge_guidance="keep separate from ctx_snapshots; snapshots capture input/context state while decision audits capture model/routing decisions over that state",
        migration_owner="astra_services::storage / context decision audit",
        product_owner="context decision explainability, prompt diagnostics, evaluation traceability",
    ),
    "eval_gate_results": TableMetadata(
        semantic_owner="astra_services::evaluation gates",
        state_class="durable evaluation gate result fact",
        primary_query="gate result lookup/list by gate_id, user_id, created_at, change_type, change_id, and passed",
        retention_policy="retain while rollout decisions, regression analysis, and change audit need pass/fail, sessions_tested, error_rate, and score_delta; prune only under evaluation history policy",
        rebuildability="not rebuildable after gate result metrics and change linkage are dropped",
        merge_guidance="keep separate from eval_quality_assessments; gate results are change-level release decisions, quality assessments are target-level scores",
        migration_owner="astra_services::storage / evaluation",
        product_owner="evaluation gates, rollout safety, regression audit",
    ),
    "eval_quality_assessments": TableMetadata(
        semantic_owner="astra_services::evaluation quality",
        state_class="durable evaluation quality assessment fact",
        primary_query="quality assessment lookup/list by assessment_id, target_id, user_id, level, updated_at, and score",
        retention_policy="retain while quality dashboards, marketplace trust, or training selection need target scores and levels; update rows through evaluation lifecycle rather than generic cleanup",
        rebuildability="not rebuildable after score, level, step_count, and target_id assessment history are lost",
        merge_guidance="keep separate from skill_metrics and eval_gate_results; this is target-level assessment state, not marketplace aggregate ranking or release gate decisions",
        migration_owner="astra_services::storage / evaluation",
        product_owner="quality assessment, trust scoring, evaluation dashboards",
    ),
    "eval_calibration_assessments": TableMetadata(
        semantic_owner="astra_services::evaluation calibration",
        state_class="durable confidence calibration fact",
        primary_query="calibration lookup/list by user_id, calibration_id, session_id, agent_id, and created_at",
        retention_policy="retain while confidence calibration, per-agent quality analysis, and session evaluation traces need paired confidence/quality_score observations",
        rebuildability="not rebuildable after confidence and quality_score observations are dropped",
        merge_guidance="keep separate from eval_quality_assessments; calibration stores observation pairs for confidence reliability, not target-level quality state",
        migration_owner="astra_services::storage / evaluation",
        product_owner="confidence calibration, agent quality analysis, evaluation traces",
    ),
    "eval_training_datasets": TableMetadata(
        semantic_owner="astra_services::evaluation datasets",
        state_class="durable evaluation training dataset fact",
        primary_query="training dataset lookup/list by dataset_id, user_id, status, created_at, and updated_at",
        retention_policy="retain while dataset_json is available for evaluation, training, or replay; delete only through dataset lifecycle so sample_count and threshold metadata stay coherent",
        rebuildability="not rebuildable after dataset_json, request_json, sample_count, and quality_threshold are lost unless the exact source generation inputs still exist",
        merge_guidance="keep separate from eval_user_feedback and eval_quality_assessments; datasets are materialized training/eval corpora, not raw feedback or assessment outputs",
        migration_owner="astra_services::storage / evaluation",
        product_owner="evaluation dataset generation, training, replay",
    ),
    "eval_user_feedback": TableMetadata(
        semantic_owner="astra_services::evaluation feedback",
        state_class="durable user feedback fact",
        primary_query="feedback lookup/list by user_id, session_id, agent_id, feedback_type, created_at, and turn_id",
        retention_policy="retain while feedback can influence quality assessment, skill evaluation, or user-visible audit; session hard delete removes owner/session rows when feedback is session-scoped",
        rebuildability="not rebuildable after rating, comment, feedback_type, and turn/session linkage are lost",
        merge_guidance="keep separate from agent_events until feedback readers stop querying rating/comment directly; feedback is evaluation input rather than timeline-only audit",
        migration_owner="astra_services::storage / evaluation",
        product_owner="user feedback, quality loops, evaluation training inputs",
    ),
    "preview_template_registry": TableMetadata(
        semantic_owner="astra_services::tool_output_preview",
        state_class="durable preview template registry fact",
        primary_query="preview template lookup by tool_name/version and active template list by tool_name/status/updated_at",
        retention_policy="retain active and compatible template versions while tool output previews can be rendered or re-normalized; deactivate old versions instead of deleting while artifacts may reference them",
        rebuildability="rebuildable only from checked-in template definitions if they exactly match deployed first_class_columns_json, field weights, and schema_json",
        merge_guidance="keep separate from raw_ref_scheme_registry; preview templates control rendering/normalization while raw ref schemes control resolver and access semantics",
        migration_owner="astra_services::storage / tool_output_preview",
        product_owner="tool output preview rendering, search normalization, artifact UX",
    ),
    "raw_ref_scheme_registry": TableMetadata(
        semantic_owner="astra_services::raw_ref_resolver",
        state_class="durable raw reference scheme registry fact",
        primary_query="raw reference scheme lookup by scheme and active resolver metadata",
        retention_policy="retain active resolver definitions while raw refs in manifests, previews, citations, or artifacts can be dereferenced; disable schemes before deleting resolver metadata",
        rebuildability="rebuildable only from resolver bootstrap definitions if access_check, backing_store, and canonical examples remain identical",
        merge_guidance="keep separate from preview_template_registry; scheme rows define dereference and access-check authority, not preview rendering templates",
        migration_owner="astra_services::storage / raw_ref_resolver",
        product_owner="raw reference resolution, artifact/context access checks, manifest dereferencing",
    ),
    "llm_provider_admission_pacing": TableMetadata(
        semantic_owner="astra_runtime::llm_provider_admission",
        state_class="coordination virtual-time pacing fact",
        primary_query="provider/provider-model pacing claim by bucket_key and tat_ms",
        retention_policy="short rolling retention with admission state; stale bucket rows can be pruned when no active admission window references the bucket",
        rebuildability="not rebuildable for current admission pacing because tat_ms is live distributed coordination state; stale rows can expire",
        merge_guidance="keep separate from llm_provider_admission_windows; pacing is virtual-time concurrency smoothing while windows enforce fixed RPM/TPM counters",
        migration_owner="astra_runtime::llm_provider_admission",
        product_owner="LLM provider admission control and dry-run/load-test safety",
    ),
    "edge_agent_registry": TableMetadata(
        semantic_owner="astra_services::multi_agent::edge_registry",
        state_class="durable edge agent registry fact",
        primary_query="edge agent registration and heartbeat lookup by user_id, edge_agent_id, registry_id, edge_id, and last_heartbeat_at",
        retention_policy="retain while an edge agent can receive dispatches or appear in status; unregister deletes the user/edge_agent_id row",
        rebuildability="rebuildable only when the edge agent reconnects and re-registers with capabilities_json/worktree metadata",
        merge_guidance="keep separate from edge_pending_dispatch; registry is liveness/capability state while dispatch owns per-request coordination",
        migration_owner="astra_services::storage / multi_agent::edge_registry",
        product_owner="edge agent status, dispatch routing, no-sticky edge recovery",
    ),
    "task_leases": TableMetadata(
        semantic_owner="astra_services::multi_agent::task_lease",
        state_class="coordination task lease fact",
        primary_query="claim/renew/release lookup by user_id, task_id, holder_agent_id, holder_edge_id, expires_at, and lease_version",
        retention_policy="cleanup_expired_data prunes expired rows after task_lease_days default 7 in ordered batches; release deletes active lease after clearing agent_tasks.agent_id",
        rebuildability="not rebuildable for current ownership; expired leases can be reclaimed but active lease_version state is live coordination",
        merge_guidance="keep separate from agent_tasks; leases are mutable worker coordination and must lock before/with task rows to avoid claim/release races",
        migration_owner="astra_services::storage / multi_agent::task_lease",
        product_owner="distributed task claim, worker heartbeat, multi-agent execution",
    ),
    "plan_templates": TableMetadata(
        semantic_owner="astra_services::task_orchestrator / state_sync",
        state_class="derived learned plan template fact",
        primary_query="template lookup and sync by user_id, template_id, goal_pattern, project_type, success_rate, and use_count",
        retention_policy="retain while learning successful patterns and edge/cloud plan template sync need reusable templates",
        rebuildability="rebuildable only by relearning from historical successful tasks while source history remains complete",
        merge_guidance="keep separate from plans; templates are reusable learned patterns, plans are per-session/user execution state",
        migration_owner="astra_services::storage / task_orchestrator",
        product_owner="plan learning, task planning, edge/cloud sync",
    ),
    "plans": TableMetadata(
        semantic_owner="runtime::server::plan_handlers / astra_services::state_sync",
        state_class="durable plan execution state",
        primary_query="plan list/get/update by user_id, plan_id, session_id, phase, updated_at, and version",
        retention_policy="retain while plan mode, rewind/redo, execution status, and edge/cloud sync need plan_json/plan_md; session hard delete removes owner/session plans after dependent plan_step_runs",
        rebuildability="not rebuildable after plan_json, plan_md, version, progress, and subtask_count are lost",
        merge_guidance="keep separate from plan_step_runs; plans own current mutable plan state while step runs are append-only attempt history",
        migration_owner="astra_services::storage / runtime plan handlers",
        product_owner="plan mode, plan execution, rewind/redo, edge sync",
    ),
    "plan_step_runs": TableMetadata(
        semantic_owner="runtime::server::plan_handlers / astra_services::state_sync",
        state_class="durable plan step attempt audit fact",
        primary_query="step attempt history by user_id, plan_id, subtask_id, attempt, run_id, request_id, and started_at",
        retention_policy="retain with parent plans while step status, retry history, artifacts, and edge/cloud sync need attempt chains; session hard delete removes rows before plans",
        rebuildability="not rebuildable after attempt numbers, request_id, error, artifact_ref, and timing are lost",
        merge_guidance="keep separate from plans; this table is append-only attempt history with unique subtask attempt semantics",
        migration_owner="astra_services::storage / runtime plan handlers",
        product_owner="plan step audit, retry/redo, execution history",
    ),
    "task_contracts": TableMetadata(
        semantic_owner="astra_services::durable_task",
        state_class="durable task acceptance contract fact",
        primary_query="active contract lookup by user_id, contract_id, task_id, status, version, and session_id",
        retention_policy="retain while durable task verification, restore, and acceptance criteria may need goal/scope/subtasks/criteria; session hard delete removes owner/session contracts after verification_results",
        rebuildability="not rebuildable after goal, scope_json, subtasks_json, criteria_json, and version are lost",
        merge_guidance="keep separate from verification_results; contracts define expected criteria, verification rows record evidence for attempts",
        migration_owner="astra_services::storage / durable_task",
        product_owner="durable task verification and acceptance criteria",
    ),
    "verification_results": TableMetadata(
        semantic_owner="astra_services::durable_task",
        state_class="durable verification evidence fact",
        primary_query="verification evidence by user_id, result_id, contract_id, subtask_id, status, created_at, and task_id",
        retention_policy="retain with task_contracts while audit/review needs pass/fail evidence; session hard delete removes rows before task_contracts",
        rebuildability="not rebuildable after evidence, expected, duration, error_message, attempt, and status are lost",
        merge_guidance="keep separate from task_contracts; evidence rows fan out per criterion/attempt and have different retention pressure",
        migration_owner="astra_services::storage / durable_task",
        product_owner="task verification evidence, audit, acceptance review",
    ),
    "wf_triggers": TableMetadata(
        semantic_owner="astra_services::triggers",
        state_class="durable workflow trigger fact",
        primary_query="trigger lookup/list by trigger_id, user_id, trigger_type, is_active, agent_id, and session_id",
        retention_policy="retain while the trigger is active or webhook/cron execution can invoke it; session hard delete removes session-scoped triggers",
        rebuildability="not rebuildable after user_input, context, cron_expr, secret, and agent/session binding are lost",
        merge_guidance="keep separate from agent_agents; triggers bind invocation policy to agents and sessions with separate activation lifecycle",
        migration_owner="astra_services::storage / triggers",
        product_owner="workflow triggers, webhook/cron automation",
    ),
    "infra_sandbox_metadata": TableMetadata(
        semantic_owner="astra_services::sandbox",
        state_class="durable sandbox metadata fact",
        primary_query="sandbox lookup/list/delete by sandbox_name, user_id, status, and created_at",
        retention_policy="retain while sandbox lifecycle, ownership checks, and list/delete APIs need metadata; delete only through sandbox service",
        rebuildability="not rebuildable after sandbox_name ownership, description, created_by, and status are lost",
        merge_guidance="keep separate from workspace_records; sandbox metadata tracks infrastructure sandboxes, workspace_records track reusable workspaces and cleanup debt",
        migration_owner="astra_services::storage / sandbox",
        product_owner="sandbox inventory and admin/user cleanup",
    ),
    "team_definitions": TableMetadata(
        semantic_owner="astra_services::team_persistence",
        state_class="durable team definition fact",
        primary_query="team create/update/get/list/delete by user_id, team_id, name, and updated_at",
        retention_policy="retain while a named team can be executed or snapshotted; delete should also consider execution history and snapshots for audit needs",
        rebuildability="not rebuildable after coordination, members_json, context_json, worktree_mode, and budget_json are lost",
        merge_guidance="keep separate from team_execution_history and team_snapshots; definitions are mutable team config, history/snapshots are execution/audit facts",
        migration_owner="astra_services::storage / team_persistence",
        product_owner="team management and multi-agent orchestration",
    ),
    "team_execution_history": TableMetadata(
        semantic_owner="astra_services::team_persistence",
        state_class="durable team execution audit fact",
        primary_query="execution history by user_id, team_id, execution_id, started_at, status, and completed_at",
        retention_policy="retain while team execution audit, result display, and debugging need result_json/status/timestamps; cleanup should be bounded by team/user history policy",
        rebuildability="not rebuildable after task, result_json, status, and timing are lost",
        merge_guidance="keep separate from team_definitions; this is append-like execution history with different retention pressure",
        migration_owner="astra_services::storage / team_persistence",
        product_owner="team execution history and result audit",
    ),
    "team_snapshots": TableMetadata(
        semantic_owner="astra_services::team_persistence",
        state_class="durable team snapshot fact",
        primary_query="snapshot lookup/list by user_id, snapshot_id, team_name, session_id, and created_at",
        retention_policy="retain while users need point-in-time team definition snapshots or git_commit/session labels for audit; delete through snapshot lifecycle",
        rebuildability="partially rebuildable only if the original team definition, git commit, and label context still exist",
        merge_guidance="keep separate from team_definitions; snapshots are point-in-time audit records and can outlive mutable team config changes",
        migration_owner="astra_services::storage / team_persistence",
        product_owner="team snapshots, audit, reproducibility",
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


P1_5_CONSOLIDATION_REVIEWS: tuple[ConsolidationReview, ...] = (
    ConsolidationReview(
        candidate="session_sync_log",
        decision="removed",
        current_read_paths=[
            "none; MatrixOneSyncService::status no longer queries audit storage",
        ],
        current_write_paths=[
            "none; SyncAuditWriter emits tracing debug events only",
        ],
        user_api_impact=(
            "sync audit is tracing-only; durable sync facts remain in domain tables, and "
            "sync status no longer depends on session_sync_log"
        ),
        migration_backfill=(
            "no backfill; schema setup drops the legacy table and live MatrixOne tests assert it is absent"
        ),
        rollback=(
            "rollback would require explicitly reintroducing the DDL and persistence path; "
            "that is intentionally not part of the current schema contract"
        ),
        test_evidence=[
            "scripts/schema/test_schema_inventory.py::test_session_sync_log_is_removed_from_production_schema",
            "crates/services/tests/services_db_integration.rs::sync_audit_no_longer_persists_session_sync_log_on_live_matrixone",
        ],
        rationale=(
            "session_sync_log was a best-effort audit side effect, not a recovery or product fact; "
            "removing it reduces schema surface without losing durable sync state"
        ),
    ),
    ConsolidationReview(
        candidate="data_versioning_checkpoints",
        decision="keep",
        current_read_paths=[
            "crates/services/src/data_versioning.rs::get_checkpoint",
            "crates/services/src/data_versioning.rs::list_checkpoints",
        ],
        current_write_paths=[
            "crates/services/src/data_versioning.rs::create_checkpoint",
        ],
        user_api_impact=(
            "data versioning rollback/list workflows depend on named checkpoint identity and created_at audit"
        ),
        migration_backfill=(
            "no deletion; if the feature is retired, first remove service/API callers and DB integration tests"
        ),
        rollback=(
            "checkpoint rows are not reconstructable after deletion unless an external version store "
            "preserved equivalent identity/name/timestamp data"
        ),
        test_evidence=[
            "crates/services/tests/data_versioning_db_it.rs",
            "scripts/schema/test_schema_inventory.py::test_p1_5_consolidation_reviews_are_evidence_backed",
        ],
        rationale=(
            "small table size is not evidence of redundancy; it is the durable root for rollback/list identity"
        ),
    ),
    ConsolidationReview(
        candidate="preview_template_registry + raw_ref_scheme_registry",
        decision="keep_separate",
        current_read_paths=[
            "crates/services/src/runs.rs::preview_template_registry",
            "crates/services/src/context_manifest.rs::raw_ref_scheme_registry",
        ],
        current_write_paths=[
            "crates/services/src/storage.rs::seed raw_ref_scheme_registry",
            "crates/services/src/storage.rs::seed preview_template_registry",
        ],
        user_api_impact=(
            "preview templates affect artifact/tool-output rendering; raw-ref schemes affect dereference "
            "authority and access checks"
        ),
        migration_backfill=(
            "no merge; a unified table would need a typed registry model and separate indexes for resolver "
            "authority versus rendering templates"
        ),
        rollback=(
            "keep current bootstrap seeds as rollback source; merged rows would need lossless split back "
            "into scheme metadata and template metadata"
        ),
        test_evidence=[
            "crates/services/tests/schema_assertions.rs::preview_template_registry",
            "crates/services/tests/schema_assertions.rs::raw_ref_scheme_registry",
            "crates/runtime/tests/phase6_artifact_preview.rs",
        ],
        rationale=(
            "same bootstrap area does not imply same lifecycle; resolver/access semantics differ from rendering"
        ),
    ),
    ConsolidationReview(
        candidate="harness_skill_drafts + harness_skill_rules",
        decision="keep_separate",
        current_read_paths=[
            "crates/services/src/harness.rs::list_skill_drafts",
            "crates/services/src/harness.rs::harness_skill_rules SELECT paths",
        ],
        current_write_paths=[
            "crates/services/src/harness.rs::create skill drafts",
            "crates/services/src/harness.rs::create/update skill rules",
        ],
        user_api_impact=(
            "Skillify draft review/publish workflow and evidence-backed rule review have distinct "
            "cardinality and decision surfaces"
        ),
        migration_backfill=(
            "no merge; a combined table would need item_type-specific constraints and would weaken "
            "rule fanout/query indexes"
        ),
        rollback=(
            "current split tables are rollback-safe; merging would require lossless split by draft/rule "
            "identity and citation references"
        ),
        test_evidence=[
            "crates/services/tests/harness_skillify_db_it.rs",
            "crates/services/tests/services_db_integration.rs::harness_skill_rules",
        ],
        rationale=(
            "rules are evidence-backed child assertions, not just optional columns on a draft"
        ),
    ),
    ConsolidationReview(
        candidate="team_execution_history + team_snapshots",
        decision="keep_separate",
        current_read_paths=[
            "crates/services/src/team_persistence.rs::list_executions_page",
            "crates/services/src/team_persistence.rs::list_snapshots_page",
        ],
        current_write_paths=[
            "crates/services/src/team_persistence.rs::record_execution_start",
            "crates/services/src/team_persistence.rs::save_snapshot",
        ],
        user_api_impact=(
            "/teams/{name}/executions and /teams/{name}/snapshots expose different resources: "
            "execution result audit versus point-in-time team definition snapshots"
        ),
        migration_backfill=(
            "no merge; both APIs now have seek pagination and different cursor keys"
        ),
        rollback=(
            "current split tables avoid backfill risk; merging would require reversible event_type mapping "
            "and separate cursor compatibility"
        ),
        test_evidence=[
            "crates/services/tests/team_persistence_integration.rs",
            "crates/runtime/tests/system_matrix_http_e2e/journey_team_snapshots_matrix.rs",
            "crates/runtime/src/server/team_handlers.rs::team handler cursor tests",
        ],
        rationale=(
            "execution history is append-like run audit; snapshots are named reproducibility artifacts"
        ),
    ),
)


def repository_root() -> Path:
    return REPO_ROOT


def discover_production_ddl_source_paths(root: Path | None = None) -> list[str]:
    root = root or REPO_ROOT
    crates_dir = root / "crates"
    discovered: list[str] = []
    for path in sorted(crates_dir.glob("*/src/**/*.rs")):
        text = production_source(path.read_text(encoding="utf-8"), stop_at_cfg_test=True)
        if CREATE_TABLE_RE.search(text):
            discovered.append(path.relative_to(root).as_posix())
    return discovered


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
        "p1_5_consolidation_reviews": [
            asdict(review) for review in P1_5_CONSOLIDATION_REVIEWS
        ],
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
