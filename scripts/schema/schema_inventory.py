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
        owner="astra_services::work",
        domain="work",
        path="crates/services/src/work.rs",
        startup_owner="ensure_core_schema via crate::work::WORK_SCHEMA_TABLES",
        state_class_hint="durable authority and immutable history, with recovery-idempotency (recovery/idempotency) facts",
        hot_path_hint="projection-sequence (projection/sequence) reads and hot coordination",
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
    r"\bCREATE\s+TABLE\s+IF\s+NOT\s+EXISTS\s+"
    r"`?([A-Za-z_][A-Za-z0-9_]*)`?\s*\(",
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
    "agent_mailbox_directory": TableMetadata(
        semantic_owner="astra_messaging::db_transport",
        state_class="leased distributed mailbox routing fact",
        primary_query="active subscriber routing by delegation_id, agent_id, instance_id, run_id, and lease_expires_at_ms",
        retention_policy="delete on unsubscribe or lease loss; prune expired registrations so senders cannot route messages to an instance that no longer polls",
        rebuildability="rebuildable only by a live subscriber registering its mailbox again",
        merge_guidance="keep separate from the message queue; directory leases select a consumer instance while queue rows own delivery and retry state",
        migration_owner="astra_messaging::db_transport",
        product_owner="distributed agent mailbox discovery and lease recovery",
    ),
    "agent_session_execution_slots": TableMetadata(
        semantic_owner="astra_services::runs::DatabaseRunStateStore",
        state_class="session-scoped execution exclusion fact",
        primary_query="single active run claim and stale-owner recovery by user_id, session_id, run_id, and updated_at",
        retention_policy="retain only while a session run owns the execution slot; release on terminal transition and remove with session hard delete",
        rebuildability="not safely rebuildable from run status during concurrent execution because it is the serialization authority",
        merge_guidance="keep separate from agent_runs; the composite session key is the storage-enforced exclusivity boundary across competing run rows",
        migration_owner="astra_services::runs",
        product_owner="session run exclusivity, resume, and crash recovery",
    ),
    "astra_schema_bootstrap_leases": TableMetadata(
        semantic_owner="astra_services::storage::SchemaBootstrapLease",
        state_class="short-lived schema migration coordination lease",
        primary_query="schema bootstrap ownership by component, holder_id, and expires_at_unix_ms",
        retention_policy="release after bootstrap and replace only after expiry; rows have no product-history retention requirement",
        rebuildability="fully rebuildable by the next schema bootstrap contender",
        merge_guidance="keep separate from schema contracts; this table coordinates writers while contract tables record installed schema identity",
        migration_owner="astra_services::storage",
        product_owner="safe concurrent Server startup and schema installation",
    ),
    "astra_schema_contracts": TableMetadata(
        semantic_owner="astra_services::storage::ensure_core_schema",
        state_class="durable installed schema contract fact",
        primary_query="installed contract version by schema component",
        retention_policy="retain for the lifetime of the component schema and replace transactionally when a newer verified contract is installed",
        rebuildability="rebuildable only after verifying every required table contract against the live database",
        merge_guidance="keep as the component-level contract identity; table-level fingerprints remain in astra_schema_table_contracts",
        migration_owner="astra_services::storage",
        product_owner="schema compatibility, startup validation, and migration recovery",
    ),
    "astra_schema_table_contracts": TableMetadata(
        semantic_owner="astra_services::storage::ensure_core_schema",
        state_class="durable per-table schema fingerprint fact",
        primary_query="installed table contract by component, contract_version, and table_name",
        retention_policy="retain the current verified table fingerprints with the component contract; replace as one schema installation unit",
        rebuildability="rebuildable from canonical DDL only after checking the live table shape",
        merge_guidance="keep separate from the component contract row because validation and diagnostics need one independently named fingerprint per table",
        migration_owner="astra_services::storage",
        product_owner="schema drift detection and migration diagnostics",
    ),
    "auth_provider_request_replay": TableMetadata(
        semantic_owner="astra_services::auth::provider request authorization",
        state_class="durable provider-request replay prevention fact",
        primary_query="authorization replay claim by provider, request_authorization_id, request_id, and expires_at_unix",
        retention_policy="retain through the signed request validity window, then prune expired rows in bounded batches",
        rebuildability="not rebuildable during the validity window without reopening replay risk",
        merge_guidance="keep separate from auth audit and bearer tokens; this table is a hot uniqueness boundary for provider-signed requests",
        migration_owner="astra_services::storage / auth",
        product_owner="provider callback authentication and replay protection",
    ),
    "inference_invocations": TableMetadata(
        semantic_owner="astra_services::inference_execution",
        state_class="durable logical inference lifecycle authority",
        primary_query="invocation ownership, status, terminal fingerprint, usage, and route lookup by user_id, invocation_id, session_id, run_id, or harness_run_id",
        retention_policy="retain with the owning session or Harness run while replay, billing, usage, recovery, and delivery reconciliation need the logical inference boundary",
        rebuildability="not rebuildable after execution because terminal identity, admitted limits, usage, and delivery outcome are authoritative facts",
        merge_guidance="keep separate from provider attempts; one logical invocation may have multiple bounded delivery attempts without changing its owner or admitted route",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="inference durability, usage attribution, billing, and recovery",
    ),
    "inference_canonical_transition_heads": TableMetadata(
        semantic_owner="astra_services::inference_execution",
        state_class="durable per-turn canonical provider transition lineage head",
        primary_query="lock or load the single current head by user_id, session_id, and turn_index, then join head_attempt_id to its exact provider-attempt payload",
        retention_policy="retain until the canonical coordinator absorbs the turn; retirement removes the head and provider payload through the absorbed turn, and session hard delete removes any remainder",
        rebuildability="not safely rebuildable while an unabsorbed provider delivery exists because message values and provider responses do not identify the sole recoverable lineage leaf",
        merge_guidance="keep separate from immutable provider attempts; the composite primary key serializes one mutable head per session turn while the unique attempt key prevents ambiguous lineage",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="provider canonical context durability, crash recovery, and fork prevention",
    ),
    "inference_provider_attempts": TableMetadata(
        semantic_owner="astra_services::inference_execution",
        state_class="durable upstream inference delivery attempt fact",
        primary_query="attempt status, provider request identity, terminal fingerprint, and usage by user_id, invocation_id, and provider_attempt_id",
        retention_policy="retain with the parent invocation through billing reconciliation, uncertain-delivery recovery, and audit; session hard delete removes attempts before invocations",
        rebuildability="not rebuildable after provider I/O because accepted, failed, cancelled, and delivery-unknown outcomes cannot be inferred safely",
        merge_guidance="keep separate from inference_invocations so retries create explicit attempts instead of overwriting logical invocation truth",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="provider delivery audit, retry safety, usage reconciliation, and latency",
    ),
    "inference_routes": TableMetadata(
        semantic_owner="astra_services::inference_execution",
        state_class="immutable admitted inference route fact",
        primary_query="resolved offering, model, placement, billing owner, and policy revisions by user_id, route_id, session_id, run_id, or harness_run_id",
        retention_policy="retain with invocations and historical usage so later policy or catalog changes cannot rewrite what actually executed",
        rebuildability="not rebuildable from the current catalog because eligibility, connection, and policy revisions may have changed",
        merge_guidance="keep separate from mutable model catalog and invocation state; routes freeze an admitted decision shared by one or more execution facts",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="model routing audit, billing attribution, and cross-client run truth",
    ),
    "maintenance_sweep_cursors": TableMetadata(
        semantic_owner="astra_runtime maintenance sweepers",
        state_class="durable incremental maintenance progress fact",
        primary_query="last processed cursor and generation by sweep_name",
        retention_policy="retain while the named sweeper is active; reset only through an explicit full-rescan operation",
        rebuildability="rebuildable by scanning from the beginning, at the cost of duplicate maintenance work",
        merge_guidance="keep one generic cursor table instead of adding per-sweeper singleton tables; payload remains bounded and contains no product state",
        migration_owner="astra_services::storage / runtime sweepers",
        product_owner="bounded compaction, retention, and crash-resumable maintenance",
    ),
    "semantic_read_observation_budgets": TableMetadata(
        semantic_owner="astra_services::semantic_read_observation_store",
        state_class="session-scoped semantic read capacity authority",
        primary_query="atomic observation count and byte reservation by user_id and session_id",
        retention_policy="retain while the session observation cache exists; update with fills/evictions and remove with session hard delete",
        rebuildability="recomputable from observation rows only while writes are quiescent; not safe to reconstruct during concurrent reservations",
        merge_guidance="keep separate from semantic_read_observations because one bounded counter row coordinates capacity across many observation entries",
        migration_owner="astra_services::storage / semantic_read_observation_store",
        product_owner="bounded semantic read reuse and admission",
    ),
    "session_artifact_references": TableMetadata(
        semantic_owner="astra_services::session_artifact_store / tool_invocation_ledger",
        state_class="durable artifact reachability reference fact",
        primary_query="artifact reachability by user_id, session_id, artifact_id, reference_kind, and reference_id",
        retention_policy="retain while any state, transcript, tool archive, or derived artifact owner references the artifact; remove references before retention deletes content",
        rebuildability="partially rebuildable only when every referencing owner and its stable artifact identity still exist",
        merge_guidance="keep separate from session_artifacts; references are many-to-many reachability facts used to prevent premature content deletion",
        migration_owner="astra_services::storage / session_artifact_store",
        product_owner="artifact retention safety, provenance, and tool-output compaction",
    ),
    "tool_invocation_archive_chunks": TableMetadata(
        semantic_owner="astra_services::tool_invocation_ledger / runtime compactor",
        state_class="durable compacted tool invocation archive fact",
        primary_query="ordered archived invocation range by user_id, session_id, run_id, chunk_index, and identity-key bounds",
        retention_policy="retain while replay, audit, artifact reachability, or session history needs compacted invocation outcomes; session hard delete removes owner/session chunks",
        rebuildability="not rebuildable after the source ledger rows are compacted away unless the referenced archive artifact remains intact",
        merge_guidance="keep separate from the hot tool_invocation_ledger; immutable bounded chunks reduce hot-table pressure and have different read and retention patterns",
        migration_owner="astra_services::storage / tool_invocation_compactor",
        product_owner="tool invocation history, compaction, replay, and artifact retention",
    ),
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
    "agent_session_lifecycle_fences": TableMetadata(
        semantic_owner="astra_services::session_lifecycle",
        state_class="durable session lifecycle and irreversible delete-intent authority",
        primary_query="session lifecycle fence by user_id/session_id and pending delete recovery by database_deleted_at/delete_requested_at",
        retention_policy="retain beyond session deletion so delayed writers cannot recreate a deleted session; pending intents are retried after the recovery grace period",
        rebuildability="not rebuildable after the session row is removed because the fence is the surviving deletion authority",
        merge_guidance="keep separate from agent_sessions; the fence must survive removal of the session aggregate and serialize every session-root writer",
        migration_owner="astra_services::storage / session_lifecycle",
        product_owner="irreversible session deletion, delayed-write fencing, and crash recovery",
    ),
    "session_deletion_tombstones": TableMetadata(
        semantic_owner="astra_services::session_lifecycle",
        state_class="durable session deletion compatibility tombstone",
        primary_query="deleted-session lookup by user_id/session_id and retention scan by deleted_at",
        retention_policy="retain as compatibility evidence while legacy readers and delayed event-ingestion paths can encounter the deleted identity",
        rebuildability="not rebuildable after deletion without the surviving lifecycle fence or another durable deletion record",
        merge_guidance="migrate consumers toward agent_session_lifecycle_fences before consolidation; do not remove while any path still checks tombstones",
        migration_owner="astra_services::storage / session_lifecycle",
        product_owner="session deletion compatibility and resurrection prevention",
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
    "semantic_read_observations": TableMetadata(
        semantic_owner="astra_services::semantic_read_observation_store",
        state_class="rebuildable session-scoped optimization fact",
        primary_query="atomic lookup/fill/complete by user_id, session_id, and content-addressed semantic key",
        retention_policy="hard bounded per session by ready entry, ready byte, and in-flight fill limits; deterministic LRU eviction; session hard delete removes all owner/session rows",
        rebuildability="fully rebuildable by executing the authorized pure read again; errors and uncertain outcomes are never stored",
        merge_guidance="keep separate from the invocation ledger: this reuses fresh successful observations across distinct logical invocation IDs and has independent capacity/eviction semantics",
        migration_owner="astra_services::storage / semantic_read_observation_store",
        product_owner="freshness-bound semantic read reuse and cache fill coordination",
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
        merge_guidance="do not merge into admin_config; this is the structured model registry, while admin_config stores small named overrides such as reasoning_offering_id",
        migration_owner="astra_services::storage / models",
        product_owner="model registry, model resolution, LLM credential administration",
    ),
    "runtime_llm_trusted_domains": TableMetadata(
        semantic_owner="astra_services::llm_trusted_domains",
        state_class="durable LLM endpoint trust policy fact",
        primary_query="trusted endpoint allowlist lookup/list by domain_host, domain_port, is_enabled, and domain_id",
        retention_policy="retain enabled/disabled policy rows while runtime endpoint validation and admin audit need the trust decision; delete only through trusted-domain admin API",
        rebuildability="rebuildable only from a separate security policy source if one exists",
        merge_guidance="keep separate from infra_llm_models; this table owns host/port trust policy, not model credentials or provider capability resolution",
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
        retention_policy="retain while session restore, rollback, tool state, and checkpoint list APIs may need state_json/tools_json; session hard delete removes owner/session rows",
        rebuildability="not rebuildable after state_json, tools_json, summary, and token/error counters are lost",
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
        primary_query="configuration lookup and list by config_key, including reasoning_offering_id selection override",
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
    "auth_memoria_identities": TableMetadata(
        semantic_owner="astra_services::auth / Memoria integration",
        state_class="durable external-to-Astra identity mapping fact",
        primary_query="resolve the stable Astra user_id by Memoria user_id during scoped-key login",
        retention_policy="retain while the linked Astra account exists; remove only through an explicit account-unlink or account-deletion workflow",
        rebuildability="deterministically rebuildable from the verified Memoria user_id, but loss can break continuity until the mapping is recreated",
        merge_guidance="keep separate from auth_users and auth_tokens; external identity mapping, local account state, and encrypted credentials have different trust and revocation boundaries",
        migration_owner="astra_services::storage / auth",
        product_owner="Memoria sign-in and Astra account continuity",
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
    "works": TableMetadata(
        semantic_owner="astra_services::work::repository",
        state_class="durable Work root and current-state authority",
        primary_query="current work root by owner_id/work_id with current goal, criteria, delivery branch, revision, and archive state",
        retention_policy="retain for the Work lifecycle and immutable revision/evidence audit; archive through archived_at and delete only with an explicit owner-scoped cleanup policy",
        rebuildability="not rebuildable from Work events because current pointers, delivery branch, revision, and archive state are the Work root authority",
        merge_guidance="keep Work root/current state separate from immutable goal, criterion, graph, and item revisions; works owns current pointers and lifecycle identity",
        migration_owner="astra_services::work",
        product_owner="Work lifecycle root, current state, and delivery branch",
    ),
    "work_goal_revisions": TableMetadata(
        semantic_owner="astra_services::work::goal",
        state_class="immutable Work goal revision history",
        primary_query="goal revision by owner_id/work_id/revision with source, actor, reason, and created_at",
        retention_policy="retain every accepted goal revision with the Work history; revisions are immutable and removed only with the owning Work archive policy",
        rebuildability="not rebuildable after goal text, source reference, accepted actor, or revision identity are lost",
        merge_guidance="keep immutable goal revisions separate from works current_goal_revision; the root points at the current revision but does not replace history",
        migration_owner="astra_services::work",
        product_owner="Work goal authority and immutable change history",
    ),
    "work_criteria": TableMetadata(
        semantic_owner="astra_services::work::criteria",
        state_class="durable Work criterion identity catalog",
        primary_query="criterion identity by owner_id/work_id/criterion_id",
        retention_policy="retain criterion identities while revisions or criterion sets reference them; remove only with Work history cleanup",
        rebuildability="not rebuildable when criterion identity is needed to resolve revision and acceptance evidence",
        merge_guidance="keep criterion identity separate from work_criterion_revisions and work_criterion_sets; identity, immutable definitions, and accepted membership have distinct authorities",
        migration_owner="astra_services::work",
        product_owner="Work criterion identity and lifecycle",
    ),
    "work_criterion_revisions": TableMetadata(
        semantic_owner="astra_services::work::criteria",
        state_class="immutable Work criterion revision history",
        primary_query="criterion definition revision by owner_id/work_id/criterion_id/revision and definition_hash",
        retention_policy="retain all criterion revisions used by accepted sets and checks; revisions are immutable through Work history cleanup",
        rebuildability="not rebuildable after definition_json, definition_hash, source reference, or revision identity are lost",
        merge_guidance="keep immutable criterion revisions separate from work_criteria current identity and work_criterion_sets accepted membership",
        migration_owner="astra_services::work",
        product_owner="Work criterion definitions and historical acceptance basis",
    ),
    "work_criterion_sets": TableMetadata(
        semantic_owner="astra_services::work::criteria",
        state_class="immutable accepted Work criterion-set authority",
        primary_query="criterion-set revision by owner_id/work_id/revision with member_manifest_hash and accepted actor",
        retention_policy="retain every accepted criterion-set revision for audit and replay; remove only with the owning Work history policy",
        rebuildability="not rebuildable after member manifest, hash, count, accepted actor, or parent revision are lost",
        merge_guidance="keep accepted set revisions separate from works.current_criteria_set_revision and individual criterion revisions; the root selects a set while history preserves each accepted manifest",
        migration_owner="astra_services::work",
        product_owner="Work acceptance criteria-set authority",
    ),
    "work_graph_revisions": TableMetadata(
        semantic_owner="astra_services::work::graph",
        state_class="immutable Work graph revision history",
        primary_query="graph revision by owner_id/work_id/revision with item/edge manifests, hash, parent, and patch reference",
        retention_policy="retain graph revisions referenced by branches, checks, proposals, and acceptance evidence; remove only with Work history cleanup",
        rebuildability="not rebuildable after item/edge manifests, hash, parent lineage, or revision identity are lost",
        merge_guidance="keep immutable graph revisions separate from work_graph_sequences and branch current_graph_revision; sequence allocates ordering while revisions preserve graph authority",
        migration_owner="astra_services::work",
        product_owner="Work dependency graph authority and immutable history",
    ),
    "work_graph_sequences": TableMetadata(
        semantic_owner="astra_services::work::graph",
        state_class="durable Work graph revision sequence authority",
        primary_query="last graph revision by owner_id/work_id",
        retention_policy="retain one sequence row for each active Work; update transactionally with graph revision creation and remove with Work cleanup",
        rebuildability="not safely rebuildable during concurrent graph writes because the sequence is the ordering authority",
        merge_guidance="keep sequence authority separate from immutable work_graph_revisions and branch projections; it allocates monotonic graph revision identity",
        migration_owner="astra_services::work",
        product_owner="Work graph ordering and concurrent revision allocation",
    ),
    "work_items": TableMetadata(
        semantic_owner="astra_services::work::items",
        state_class="durable Work item current-state authority",
        primary_query="current item by owner_id/work_id/item_id with last_revision",
        retention_policy="retain item identity and current revision while Work branches or evidence reference it; clean up with Work lifecycle",
        rebuildability="not rebuildable as current item authority when the latest revision pointer is lost",
        merge_guidance="keep item current state separate from immutable work_item_revisions and work_item_edges; work_items points to current content while revisions preserve history",
        migration_owner="astra_services::work",
        product_owner="Work item identity and current revision selection",
    ),
    "work_item_revisions": TableMetadata(
        semantic_owner="astra_services::work::items",
        state_class="immutable Work item revision history",
        primary_query="item revision by owner_id/work_id/item_id/revision with objective, expected result, declaration state, and parent",
        retention_policy="retain item revisions referenced by graph, attempts, checks, proposals, and acceptance; remove only with Work history cleanup",
        rebuildability="not rebuildable after objective, expected result, source reference, declaration state, or parent lineage are lost",
        merge_guidance="keep immutable item revisions separate from work_items current pointers and work_item_attempts evidence; current state and execution evidence must not overwrite history",
        migration_owner="astra_services::work",
        product_owner="Work item declaration and immutable change history",
    ),
    "work_item_edges": TableMetadata(
        semantic_owner="astra_services::work::graph",
        state_class="immutable graph dependency edge fact",
        primary_query="dependency edges by owner_id/work_id/graph_revision and predecessor/successor item ids",
        retention_policy="retain edges with their graph revision while branches, checks, and replay can reference that graph; remove only with graph history cleanup",
        rebuildability="rebuildable from an identical graph manifest only while the source graph revision remains authoritative",
        merge_guidance="keep graph edges separate from work_item_revisions and work_graph_sequences; edges are revision-scoped dependency evidence, not item content or sequence allocation",
        migration_owner="astra_services::work",
        product_owner="Work dependency graph traversal and validation",
    ),
    "work_branches": TableMetadata(
        semantic_owner="astra_services::work::branches",
        state_class="durable Work branch root and current-state authority",
        primary_query="current branch by owner_id/work_id/branch_id with session, basis/current graph revision, archive, and deletion markers",
        retention_policy="retain branch root and lineage while branch sessions, operations, and evidence need recovery; archive and delete only through the branch deletion operation",
        rebuildability="not rebuildable from branch events because current branch revision, session binding, graph pointers, and deletion fencing are authority",
        merge_guidance="keep branch root/current state separate from immutable graph/item revisions and branch operation history; work_branches owns the live branch boundary",
        migration_owner="astra_services::work",
        product_owner="Work branch lifecycle, lineage, and current graph selection",
    ),
    "work_branch_creation_operations": TableMetadata(
        semantic_owner="astra_services::work::branches",
        state_class="durable branch creation recovery and idempotency authority",
        primary_query="branch creation operation by owner/work/origin branch/operation_id and idempotency_hash, state, executor lease, and outcome",
        retention_policy="retain pending and terminal operations through retry/recovery and audit; prune only after branch lineage and session cleanup are complete",
        rebuildability="not rebuildable during recovery because operation idempotency, expected/observed revision, child identity, and executor fencing are authoritative",
        merge_guidance="keep branch creation operations separate from work_branches and branch events; branch operations are recovery/idempotency authority, while the branch row is current state",
        migration_owner="astra_services::work",
        product_owner="Work branch creation recovery and idempotent execution",
    ),
    "work_branch_control_operations": TableMetadata(
        semantic_owner="astra_services::work::branches",
        state_class="durable branch control recovery and fencing authority",
        primary_query="branch control operation by owner/work/branch/operation_id, idempotency_hash, expected writer epoch, executor lease, and outcome",
        retention_policy="retain pending and terminal control operations through retry, takeover, conflict diagnosis, and audit; prune after branch lifecycle cleanup",
        rebuildability="not rebuildable during concurrent control because expected/observed branch revision, writer epoch, and idempotency are recovery authority",
        merge_guidance="keep branch control operations separate from work_branches and other operation kinds; this row is the idempotency/recovery/fencing authority for acquire, takeover, and release",
        migration_owner="astra_services::work",
        product_owner="Work branch writer coordination and forced takeover recovery",
    ),
    "work_branch_deletion_operations": TableMetadata(
        semantic_owner="astra_services::work::branches",
        state_class="durable branch deletion recovery and idempotency authority",
        primary_query="branch deletion operation by owner/work/branch/operation_id, idempotency_hash, operation phase, executor lease, and outcome",
        retention_policy="retain deletion operations until terminal cleanup and conflict audit complete; remove only after branch/session lineage cleanup is durable",
        rebuildability="not rebuildable during deletion recovery because expected/observed revisions, phase, outcome, and executor fencing are authoritative",
        merge_guidance="keep deletion operations separate from work_branches; branch operations own idempotency/recovery while the root row owns deletion markers and current state",
        migration_owner="astra_services::work",
        product_owner="Work branch deletion, lineage garbage collection, and recovery",
    ),
    "work_branch_subjects": TableMetadata(
        semantic_owner="astra_services::work::branches",
        state_class="durable branch-selected immutable subject authority",
        primary_query="the single materialized subject selected by owner_id/work_id/branch_id with subject record revision, branch/graph revision, subject_ref, and subject_revision",
        retention_policy="retain one selected subject row per branch while the branch is active or its evidence needs the basis; replace transactionally and delete with branch cleanup, without accumulating mutable current-head history",
        rebuildability="not rebuildable as Work's selected-subject authority from mutable branch/work declarations; the workspace/materialization service remains the authority for the referenced immutable subject",
        merge_guidance="keep branch subjects separate from work_branches and immutable item/graph revisions; this row owns Work's selected immutable subject while workspace/materialization owns referenced content",
        migration_owner="astra_services::work",
        product_owner="Work branch subject targeting and invalidation",
    ),
    "work_patch_artifacts": TableMetadata(
        semantic_owner="astra_services::work::patches",
        state_class="immutable Work patch artifact evidence",
        primary_query="patch artifact by owner/work/patch_artifact_id, session, payload artifact, hash, and branch",
        retention_policy="retain patch payload identity and hashes while materialization, commit, checks, or acceptance reference them; delete with Work evidence cleanup",
        rebuildability="not rebuildable after patch hash, payload artifact identity, or source/target basis are lost",
        merge_guidance="keep patch artifacts separate from patch materialization/commit operations; artifacts preserve evidence while operations own recovery and idempotency",
        migration_owner="astra_services::work",
        product_owner="Work patch evidence, integrity, and review",
    ),
    "work_patch_materialization_operations": TableMetadata(
        semantic_owner="astra_services::work::patches",
        state_class="durable patch materialization recovery and idempotency authority",
        primary_query="materialization operation by owner/work/operation_id, request_id, target/source branch, phase, recovery_after, executor lease, and outcome",
        retention_policy="retain pending and terminal operations through retries, recovery_after scheduling, conflict diagnosis, and audit; prune after patch evidence and branch cleanup",
        rebuildability="not rebuildable during recovery because request identity, phase, expected/observed revisions, and executor fencing are authoritative",
        merge_guidance="keep patch materialization operations separate from patch artifacts and commit operations; patch operations are the idempotency/recovery authority for each distinct lifecycle",
        migration_owner="astra_services::work",
        product_owner="Work patch application recovery and idempotent materialization",
    ),
    "work_patch_commit_operations": TableMetadata(
        semantic_owner="astra_services::work::patches",
        state_class="durable patch commit recovery and idempotency authority",
        primary_query="commit operation by owner/work/operation_id, request_id, active target branch, phase, recovery_after, executor lease, and outcome",
        retention_policy="retain pending and terminal commit operations through retry, recovery, target fencing, and audit; prune after commit evidence and branch lifecycle cleanup",
        rebuildability="not rebuildable during recovery because request identity, target uniqueness, expected revisions, phase, and executor lease are authoritative",
        merge_guidance="keep patch commit operations separate from materialization operations and patch artifacts; commit has its own idempotency/recovery and active-target boundary",
        migration_owner="astra_services::work",
        product_owner="Work patch commit recovery and target coordination",
    ),
    "work_proposal_sequences": TableMetadata(
        semantic_owner="astra_services::work::proposals",
        state_class="durable Work proposal ordering authority",
        primary_query="last proposal sequence by owner_id/work_id/branch_id",
        retention_policy="retain one sequence row per active Work branch and update transactionally with proposal creation; remove with branch cleanup",
        rebuildability="not safely rebuildable during concurrent proposal writes because sequence is the ordering authority",
        merge_guidance="keep proposal sequence separate from work_proposals; sequence allocation orders proposals while proposal rows preserve lifecycle and evidence",
        migration_owner="astra_services::work",
        product_owner="Work proposal ordering and concurrency",
    ),
    "work_proposals": TableMetadata(
        semantic_owner="astra_services::work::proposals",
        state_class="durable Work proposal lifecycle authority",
        primary_query="proposal by owner/work/branch/proposal_id and proposal_seq, status, expiry, and basis revisions",
        retention_policy="retain pending proposals and exactly WORK_PROPOSAL_RETAINED_TERMINAL_PER_BRANCH=64 terminal proposals per branch; prune terminal proposals at the bounded sequence floor after acceptance/rejection/expiry",
        rebuildability="not rebuildable after proposal payload, basis hashes, sequence, status, or expiry are lost",
        merge_guidance="keep proposals separate from proposal sequences and acceptance decisions; proposal lifecycle, ordering, and acceptance evidence have distinct authorities",
        migration_owner="astra_services::work",
        product_owner="Work plan proposal lifecycle and review",
    ),
    "work_check_runs": TableMetadata(
        semantic_owner="astra_services::work::checks",
        state_class="immutable Work check execution evidence",
        primary_query="check result by owner/work/check_run_id, branch, item attempt, criterion revision, and produced_at",
        retention_policy="retain check-run detail while its source check_recorded event is inside the fixed Work event retention window; delete detail as that source event leaves the retention window, while bounded current acceptance facts preserve required hashes",
        rebuildability="not rebuildable after command/test/artifact output, basis revisions, status, and produced_at are lost",
        merge_guidance="keep check evidence separate from acceptance decisions and item attempts; checks preserve observed evidence while acceptance records the decision over it",
        migration_owner="astra_services::work",
        product_owner="Work checks, verification evidence, and freshness",
    ),
    "work_acceptance_decisions": TableMetadata(
        semantic_owner="astra_services::work::acceptance",
        state_class="immutable Work acceptance decision evidence",
        primary_query="acceptance decision by owner/work/decision_id, branch, decision_event_seq, and decided_at",
        retention_policy="retain acceptance-decision detail while its source gaps_accepted event is inside the fixed Work event retention window; delete detail as that source event leaves the retention window, while bounded current gap facts preserve current acceptance",
        rebuildability="not rebuildable after decision outcome, basis hashes, actor, and decision time are lost",
        merge_guidance="keep acceptance decisions separate from check runs, gap acceptances, and current Work pointers; decisions preserve evidence-backed authority rather than a replaceable projection",
        migration_owner="astra_services::work",
        product_owner="Work acceptance and release decisions",
    ),
    "work_current_gap_acceptances": TableMetadata(
        semantic_owner="astra_services::work::acceptance",
        state_class="durable current criterion-gap acceptance authority",
        primary_query="current gap acceptance by owner/work/branch/criterion_id with decision event sequence, status, and basis",
        retention_policy="retain this bounded current fact and current acceptance while the branch and criterion remain active, even after source events and check/acceptance detail leave the retention window; replace through a new decision event and clean up with Work history",
        rebuildability="not rebuildable as current acceptance authority after the current basis, decision, or event sequence is lost",
        merge_guidance="keep current gap acceptance separate from immutable acceptance decisions and criteria revisions; it is the current branch projection selected by explicit decision evidence",
        migration_owner="astra_services::work",
        product_owner="Work current gap handling and acceptance policy",
    ),
    "work_event_sequences": TableMetadata(
        semantic_owner="astra_services::work::events",
        state_class="durable Work event ordering and retained_from_event_seq coverage-floor authority",
        primary_query="last_event_seq and retained_from_event_seq coverage floor by owner_id/work_id; the floor defines the fixed retained event window",
        retention_policy="retain one sequence row per Work and update atomically with canonical event append; retained_from_event_seq is the coverage-floor authority for detail pruning and replay gaps",
        rebuildability="not safely rebuildable during concurrent event append because last_event_seq ordering and retained_from_event_seq coverage are authority",
        merge_guidance="keep event sequence separate from work_events and runtime outbox; sequence allocation orders canonical history without becoming event payload or runtime projection state",
        migration_owner="astra_services::work",
        product_owner="Work canonical event ordering and append concurrency",
    ),
    "work_attention_receipts": TableMetadata(
        semantic_owner="astra_services::work::attention",
        state_class="durable Work attention delivery receipt fact",
        primary_query="current attention receipt by owner_id/work_id with receipt kind, event sequence, and consumer state",
        retention_policy="retain the latest receipt while attention delivery/recovery needs it; replace idempotently and remove with Work lifecycle cleanup",
        rebuildability="not rebuildable during delivery recovery after receipt identity and acknowledged sequence are lost",
        merge_guidance="keep attention receipts separate from canonical work_events and runtime outbox coordination; receipts record consumer acknowledgment rather than event history",
        migration_owner="astra_services::work",
        product_owner="Work attention routing and delivery recovery",
    ),
    "work_item_attempts": TableMetadata(
        semantic_owner="astra_services::work::execution",
        state_class="mutable durable Work item attempt lifecycle and settlement authority",
        primary_query="attempt by owner/work/branch/item/revision/attempt_id with executor, status, mode, and started_at",
        retention_policy="retain attempts through check, acceptance, billing, and recovery audit; remove only with Work evidence cleanup after terminal reconciliation",
        rebuildability="not rebuildable after attempt identity, lifecycle status, executor identity, or terminal result/settlement are lost",
        merge_guidance="keep attempts separate from work_items/current revisions and check runs; attempts preserve execution evidence while checks preserve validation evidence",
        migration_owner="astra_services::work",
        product_owner="Work execution attempts, retries, and audit",
    ),
    "work_terminal_cuts": TableMetadata(
        semantic_owner="astra_services::work::attempt_settlement",
        state_class="durable single-winner terminal Work graph cut authority",
        primary_query="terminal cut by owner/work/branch/graph revision or unique attempt identity",
        retention_policy="retain with the Work branch settlement history; remove only with coordinated Work evidence cleanup",
        rebuildability="not safely rebuildable after concurrent settlement because the unique row is the winner authority",
        merge_guidance="keep separate from work_item_attempts; the branch primary key and attempt uniqueness reject competing terminal deliveries independently of mutable attempt state",
        migration_owner="astra_services::work",
        product_owner="Work terminal settlement concurrency and exactly-one delivery",
    ),
    "work_events": TableMetadata(
        semantic_owner="astra_services::work::events",
        state_class="immutable canonical history of Work events in a fixed retained window",
        primary_query="canonical event by owner_id/work_id/event_seq, branch_id, event_kind, and source_ref within the fixed retained window",
        retention_policy="retain exactly WORK_EVENT_RETENTION_PER_WORK=10,000 canonical events per Work; prune each event and its check/acceptance detail as it leaves the fixed retention window",
        rebuildability="not rebuildable after event payload, sequence, payload_hash, or source identity are lost",
        merge_guidance="keep work_events as canonical history separate from work_runtime_event_outbox coordination; canonical events own replay truth while the outbox owns runtime projection delivery",
        migration_owner="astra_services::work",
        product_owner="Work canonical history, replay, and audit",
    ),
    "work_runtime_event_outbox": TableMetadata(
        semantic_owner="astra_services::work::runtime_event_outbox",
        state_class="durable authoritative Run transaction runtime event fixed-size ring",
        primary_query="pending runtime event by owner/work/runtime_event_seq, event_kind, source_ref, and ring coverage relative to enqueued/projected sequences",
        retention_policy="authoritative Run transaction writes a fixed 1024-row ring; prune unprojected rows when the ring advances and surface runtime_events_expired when projection falls behind its coverage",
        rebuildability="not rebuildable from work_events; the authoritative Run transaction ring may expire before canonical projection and must report runtime_events_expired rather than inventing or silently dropping status",
        merge_guidance="keep runtime outbox separate from work_events canonical history and outbox slots; it coordinates runtime event projection delivery without becoming the Work history authority",
        migration_owner="astra_services::work",
        product_owner="Work runtime event projection and delivery recovery",
    ),
    "work_runtime_event_outbox_slots": TableMetadata(
        semantic_owner="astra_services::work::runtime_event_outbox",
        state_class="durable runtime outbox enqueued/projected sequence and coverage coordination authority",
        primary_query="enqueued and projected sequence plus pending/coverage state by owner_id/work_id",
        retention_policy="retain one slot per Work while runtime projection is active; update transactionally with ring coverage and remove with Work cleanup",
        rebuildability="not generally rebuildable during runtime projection recovery because enqueued/projected sequence and coverage are coordination authority",
        merge_guidance="keep outbox slots separate from canonical work_events and outbox rows; slots are hot coordination/projection sequence state, not immutable history or delivery payload",
        migration_owner="astra_services::work",
        product_owner="Work runtime projection hot coordination and sequencing",
    ),
    "auth_reauthentication_proofs": TableMetadata(
        semantic_owner="astra_services::auth::reauthentication",
        state_class="durable reauthentication trust fact",
        primary_query="unconsumed proof by user_id, proof_id, purpose, proof_hash, and expires_at",
        retention_policy="retain until consumed or expired, then prune expired proofs in bounded batches; expiry is part of the trust contract",
        rebuildability="not rebuildable after proof_hash and expiry are lost because those values establish the reauthentication trust fact",
        merge_guidance="keep separate from bearer tokens and device challenges; reauthentication proof expiry and purpose are a distinct trust boundary",
        migration_owner="astra_services::storage / auth",
        product_owner="reauthentication and step-up authorization",
    ),
    "conversation_segments": TableMetadata(
        semantic_owner="astra_services::context::conversation_segments",
        state_class="immutable content-addressed conversation segment fact",
        primary_query="segment content by isolation_domain, owner_user_id, segment_hash, or canonical_root_hash",
        retention_policy="retain while a manifest or transcript references the canonical segment; remove unreferenced content only through bounded context cleanup",
        rebuildability="rebuildable only from the original canonical transcript bytes, not from a current projection or manifest head",
        merge_guidance="keep separate from conversation_manifest_nodes and conversation_manifest_segments; content-addressed payloads, manifest graph nodes, and ordered links have different authorities",
        migration_owner="astra_services::storage / context",
        product_owner="canonical conversation context storage and deduplicated segment reuse",
    ),
    "conversation_manifest_nodes": TableMetadata(
        semantic_owner="astra_services::context::manifest",
        state_class="durable immutable context manifest graph authority",
        primary_query="manifest root and parent lineage by owner session/branch, conversation_seq, compaction_generation, and reachable state",
        retention_policy="retain reachable manifests and their lineage while session context can be restored; garbage-collect unreachable nodes only after pins and heads no longer reference them",
        rebuildability="not rebuildable from session_context_heads alone because parent lineage, root hashes, and reachability are manifest authority",
        merge_guidance="keep separate from session_context_heads, manifest segments, events, and receipts; the manifest graph is immutable context authority, not a current projection or operation acknowledgment",
        migration_owner="astra_services::storage / context",
        product_owner="canonical context manifest lineage, compaction, and recovery",
    ),
    "conversation_manifest_pins": TableMetadata(
        semantic_owner="astra_services::context::manifest_gc",
        state_class="durable manifest retention pin fact",
        primary_query="active pin by owner parent session/branch, manifest_root, pin_state, and grace_expires_at_ms",
        retention_policy="retain pins through their grace window and while the pinned manifest is needed for fork or recovery; expire and prune pins in bounded batches",
        rebuildability="not safely rebuildable while a pin protects context from garbage collection because pin intent and expiry are trust facts",
        merge_guidance="keep separate from manifest nodes and session context heads; pins fence cleanup without becoming the current context projection",
        migration_owner="astra_services::storage / context",
        product_owner="context retention pins, fork safety, and manifest garbage collection",
    ),
    "conversation_manifest_segments": TableMetadata(
        semantic_owner="astra_services::context::manifest",
        state_class="durable ordered manifest-to-segment projection",
        primary_query="ordered segment links by owner session/branch, manifest_root, and segment_position",
        retention_policy="retain with each reachable manifest root; delete links only after the manifest is unreachable and pins/heads permit cleanup",
        rebuildability="rebuildable from the immutable manifest node and segment content when both remain available",
        merge_guidance="keep separate from conversation_segments and manifest nodes; ordered fanout is a projection of a manifest root, not segment content or graph lineage",
        migration_owner="astra_services::storage / context",
        product_owner="ordered context materialization and manifest segment traversal",
    ),
    "inference_invocation_settlement_debts": TableMetadata(
        semantic_owner="astra_services::inference_execution::settlement",
        state_class="durable inference settlement recovery authority",
        primary_query="settlement debt by user_id, invocation_id, optional exact provider_attempt_id plus provider-delivery authorization, session_id or harness_run_id, and terminal_fingerprint",
        retention_policy="retain until usage, billing, and terminal delivery settlement is acknowledged; retry pending rows with a per-row eligibility deadline, quarantine permanent conflicts outside active recovery batches, and prune only after authoritative reconciliation",
        rebuildability="not rebuildable while unsettled because this is the recovery authority for logical invocation settlement and any acknowledged exact provider-attempt terminal",
        merge_guidance="keep separate from inference_invocations, provider attempts, and metric shards; optional attempt identity plus delivery authorization distinguishes a legitimate absent pre-delivery row from loss of an already-authorized physical request",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="inference billing, usage settlement, and uncertain-delivery recovery",
    ),
    "model_request_context_events": TableMetadata(
        semantic_owner="astra_services::inference_execution::request_context",
        state_class="append-only model request context evidence fact",
        primary_query="accepted or terminal request context by user_id, attempt_id, invocation_id, session/harness owner, and event_stage",
        retention_policy="retain append-only accepted and terminal evidence through inference audit, usage reconciliation, and recovery; delete with the owning session or harness history",
        rebuildability="not rebuildable after topology, provider/model context, token counts, and terminal status are lost",
        merge_guidance="keep append-only request context events separate from model_request_metric_shards; events preserve evidence while shards are a rebuildable aggregate projection",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="request context evidence, token accounting, and inference diagnostics",
    ),
    "model_request_metric_shards": TableMetadata(
        semantic_owner="astra_services::inference_execution::metrics",
        state_class="rebuildable sharded request metrics projection",
        primary_query="low-cardinality terminal metrics by metric_shard, topology, provider, model_family, purpose, and terminal_status",
        retention_policy="retain current aggregate shards for scraper and dashboard windows; rebuild or reset them from request context events during bounded maintenance",
        rebuildability="rebuildable from model_request_context_events terminal evidence; shard rows are not the request history authority",
        merge_guidance="keep separate from append-only request context events and settlement debts; sharding protects hot metric updates and does not replace evidence or recovery authority",
        migration_owner="astra_services::storage / inference_execution",
        product_owner="low-cardinality inference metrics and scraper performance",
    ),
    "session_attachment_quarantines": TableMetadata(
        semantic_owner="astra_services::context::attachments",
        state_class="durable attachment quarantine decision fact",
        primary_query="quarantine operation by owner session/branch, quarantine_id, idempotency_hash, and observed/current manifest roots",
        retention_policy="retain quarantine evidence through attachment recovery and audit; prune with the owning session after the operation is resolved",
        rebuildability="not rebuildable after observed manifest root, request hash, reason, and idempotency identity are lost",
        merge_guidance="keep separate from session_attachments and handoff slots; quarantine records a rejected or fenced attachment operation while attachments own active placement",
        migration_owner="astra_services::storage / context",
        product_owner="attachment trust fencing, quarantine, and recovery",
    ),
    "session_attachments": TableMetadata(
        semantic_owner="astra_services::context::attachments",
        state_class="durable attachment placement and trust fact",
        primary_query="active attachment by owner session/branch, attachment_id, attachment_epoch, idempotency_hash, and expiry",
        retention_policy="retain active attachments until expires_at_ms or explicit removal, then clean up in bounded batches with the owning session",
        rebuildability="not rebuildable during an active lease because placement, epoch, idempotency, and observed manifest root fence attachment trust",
        merge_guidance="keep separate from quarantine, handoffs, and context manifests; active attachment placement is a trust/fencing fact rather than an operation event or projection",
        migration_owner="astra_services::storage / context",
        product_owner="session attachment placement, expiry, and authorization fencing",
    ),
    "session_context_authority_events": TableMetadata(
        semantic_owner="astra_services::context::authority",
        state_class="append-only context authority and fencing evidence fact",
        primary_query="context operation outcome by owner session/branch, event_id, operation_kind, writer_epoch, and observed root",
        retention_policy="retain authority events through context replay, conflict diagnosis, and security audit; remove only with the owning session's bounded hard-delete policy",
        rebuildability="not rebuildable after writer/device/authorization/permission epochs and observed roots are lost",
        merge_guidance="keep separate from context heads, manifests, operation receipts, and transcript projections; events preserve fencing evidence while those tables own other authority surfaces",
        migration_owner="astra_services::storage / context",
        product_owner="context write authority, fencing, and conflict audit",
    ),
    "session_context_heads": TableMetadata(
        semantic_owner="astra_services::context::heads",
        state_class="mixed current context projection plus live coordination/fencing authority",
        primary_query="current context head by owner session/branch, latest_manifest_root, canonical_root_hash, completed_turn, and sequence counters",
        retention_policy="retain the current head while the session/branch is live; replace transactionally and delete with the owning session after manifest/event cleanup",
        rebuildability="manifest/sequence projection and root are repairable from manifests, authority events, and receipts; while active writer/reservation leases, expiry, writer_epoch, or authority epochs are live, the entire row is not safely rebuildable",
        merge_guidance="keep separate from manifest/events/receipts: heads are a current projection, manifests own canonical lineage, events own authority history, and receipts own idempotency outcomes; active writer, reservation, and fencing coordination remain distinct from repairable projection state",
        migration_owner="astra_services::storage / context",
        product_owner="current context projection, sequence checkpoints, and writer fencing",
    ),
    "session_context_operation_receipts": TableMetadata(
        semantic_owner="astra_services::context::authority",
        state_class="durable context operation idempotency receipt fact",
        primary_query="operation receipt by owner session/branch, operation_kind, idempotency_hash, and request_hash",
        retention_policy="retain receipts for the idempotency/recovery window and while the operation may be retried; delete only with the owning session's operation history",
        rebuildability="not rebuildable during the retry window because the receipt is the idempotency authority for an accepted context operation",
        merge_guidance="keep separate from context authority events and current heads; receipts answer idempotent retry lookups while events preserve append-only decisions and heads project current state",
        migration_owner="astra_services::storage / context",
        product_owner="context operation idempotency and recovery",
    ),
    "session_device_challenges": TableMetadata(
        semantic_owner="astra_services::auth::device_trust",
        state_class="durable expiring device challenge trust fact",
        primary_query="unconsumed challenge by user_id, challenge_id, device/session, purpose, and expires_at",
        retention_policy="retain until consumed or expiry, then prune expired challenges in bounded batches; expiry is part of the device trust fact",
        rebuildability="not rebuildable after challenge_digest, device identity, purpose, or expiry are lost",
        merge_guidance="keep separate from reauthentication proofs and device leases; challenge expiry establishes trust while leases represent current device ownership",
        migration_owner="astra_services::storage / auth",
        product_owner="device challenge verification and trust establishment",
    ),
    "session_fork_events": TableMetadata(
        semantic_owner="astra_services::context::forks",
        state_class="append-only immutable fork transition history",
        primary_query="fork transition by owner fork_id, transition_seq, parent_session_id, child_session_id, and created_at",
        retention_policy="retain with parent and child session history for replay and audit; delete only through session fork cleanup after lifecycle closure",
        rebuildability="not rebuildable after transition sequence, from/to state, and event payload are lost",
        merge_guidance="keep separate from session_forks; the fork row owns current lifecycle/idempotency while events preserve immutable transition history",
        migration_owner="astra_services::storage / context",
        product_owner="fork lifecycle audit and recovery replay",
    ),
    "session_forks": TableMetadata(
        semantic_owner="astra_services::context::forks",
        state_class="durable fork lifecycle and idempotency authority",
        primary_query="fork state by owner fork_id, parent/child session and branch, and idempotency_hash",
        retention_policy="retain while fork activation or recovery can be retried and while parent/child lineage is auditable; clean up only after both session lifecycles permit it",
        rebuildability="not rebuildable from fork events alone because current state, manifest snapshot, child uniqueness, and idempotency are lifecycle authority",
        merge_guidance="keep separate from session_fork_events and context manifests; fork operations own recovery/idempotency while event rows own immutable transition history",
        migration_owner="astra_services::storage / context",
        product_owner="session fork lifecycle, idempotency, and lineage recovery",
    ),
    "session_handoff_events": TableMetadata(
        semantic_owner="astra_services::context::handoff",
        state_class="append-only immutable handoff transition history",
        primary_query="handoff transition by owner session/branch, handoff_id, transition_seq, and created_at",
        retention_policy="retain with handoff and session audit through recovery; delete only after the handoff lifecycle and owning session are closed",
        rebuildability="not rebuildable after transition sequence, request hash, and event payload are lost",
        merge_guidance="keep separate from session_handoffs and handoff slots; events preserve immutable history, handoffs own lifecycle/idempotency, and slots own current coordination",
        migration_owner="astra_services::storage / context",
        product_owner="session handoff transition audit and recovery replay",
    ),
    "session_handoff_slots": TableMetadata(
        semantic_owner="astra_services::context::handoff",
        state_class="durable handoff slot and attachment epoch coordination fact",
        primary_query="current active_handoff_id and next_attachment_epoch by owner session/branch",
        retention_policy="retain one slot per live session/branch and update transactionally; release with session cleanup after handoff closure",
        rebuildability="not safely rebuildable during concurrent handoff or attachment writes because the slot is a fencing and epoch authority",
        merge_guidance="keep separate from handoffs, handoff events, and attachments; the slot serializes current coordination while those tables preserve lifecycle/history or placement",
        migration_owner="astra_services::storage / context",
        product_owner="handoff coordination, attachment epochs, and fencing",
    ),
    "session_handoffs": TableMetadata(
        semantic_owner="astra_services::context::handoff",
        state_class="durable handoff lifecycle and idempotency authority",
        primary_query="handoff state by owner session/branch, handoff_id, idempotency_hash, state, and deadline_ms",
        retention_policy="retain active and recently terminal handoffs through deadline-based recovery and audit; prune only after event history and session lifecycle permit it",
        rebuildability="not rebuildable from handoff events alone because current state, deadline, record, and idempotency are lifecycle authority",
        merge_guidance="keep separate from handoff events and handoff slots; lifecycle/idempotency, immutable history, and hot current coordination have separate boundaries",
        migration_owner="astra_services::storage / context",
        product_owner="session handoff lifecycle and recovery",
    ),
    "session_transcript_projection_heads": TableMetadata(
        semantic_owner="astra_services::context::transcript_projection",
        state_class="durable transcript projection sequence head",
        primary_query="transcript projection head by user_id/session_id, completed_turn, journal_event_seq, conversation_seq, and canonical_root_hash",
        retention_policy="retain the current projection checkpoint while transcript replay and compaction need it; replace during projection repair and remove with session cleanup",
        rebuildability="rebuildable from transcript items and canonical journal/manifests when sequence and root evidence remain available",
        merge_guidance="keep separate from session_context_heads, transcript items, manifests, and events; this is transcript projection state keyed by projection sequence, not canonical history",
        migration_owner="astra_services::storage / context",
        product_owner="transcript materialization, compaction, and projection recovery",
    ),
    "session_weighted_admission_gates": TableMetadata(
        semantic_owner="astra_services::context::weighted_admission",
        state_class="durable hot admission coordination gate",
        primary_query="scope-level admission gate by scope_name and updated_at",
        retention_policy="retain one gate per configured scope while admission coordination is enabled; recreate or remove with scope configuration",
        rebuildability="fully rebuildable by admission bootstrap because it is coordination state rather than context history",
        merge_guidance="keep separate from weighted reservations; the gate serializes scope coordination while reservations own capacity claims and expiry",
        migration_owner="astra_services::storage / context",
        product_owner="weighted context admission hot coordination",
    ),
    "session_weighted_admission_reservations": TableMetadata(
        semantic_owner="astra_services::context::weighted_admission",
        state_class="durable capacity reservation and fencing fact",
        primary_query="active reservation by scope_name, reservation_id, owner session/branch, idempotency_hash, and expires_at",
        retention_policy="retain reservations until expiry or release, then prune expired capacity claims in bounded batches",
        rebuildability="not safely rebuildable during an active admission window because reserved bytes/tokens/slots and idempotency are capacity authority",
        merge_guidance="keep separate from weighted gates and context heads; reservations own trust/fencing/capacity claims while gates coordinate and heads project context",
        migration_owner="astra_services::storage / context",
        product_owner="weighted admission capacity, idempotency, and recovery",
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
    text = strip_rust_comments(text)
    if stop_at_cfg_test:
        cfg_test_index = first_cfg_test_marker(text)
        if cfg_test_index is not None:
            text = text[:cfg_test_index]
    return text


def strip_rust_line_comments(text: str) -> str:
    """Remove full-line Rust comments while preserving source line numbers.

    Kept as a small compatibility helper for callers that used the original
    line-comment filter.  ``production_source`` uses the slightly more
    complete scanner below so an inline or block comment cannot contribute a
    false CREATE TABLE match.
    """

    kept: list[str] = []
    for line in text.splitlines():
        if line.lstrip().startswith("//"):
            kept.append("")
        else:
            kept.append(line)
    return "\n".join(kept)


def raw_string_prefix(chars: list[str], index: int) -> tuple[int, int] | None:
    """Return (hash count, closing-prefix index) for a Rust raw string start."""

    if index > 0 and (chars[index - 1].isalnum() or chars[index - 1] == "_"):
        return None
    raw_index = index
    if chars[index] == "b" and index + 1 < len(chars) and chars[index + 1] == "r":
        raw_index += 1
    elif chars[index] != "r":
        return None
    cursor = raw_index + 1
    while cursor < len(chars) and chars[cursor] == "#":
        cursor += 1
    if cursor >= len(chars) or chars[cursor] != '"':
        return None
    return cursor - raw_index - 1, cursor


def first_cfg_test_marker(text: str) -> int | None:
    """Find an active cfg(test) attribute without matching string contents."""

    chars = list(text)
    index = 0
    quote: str | None = None
    raw_hashes: int | None = None
    escaped = False
    while index < len(chars):
        char = chars[index]
        if raw_hashes is not None:
            if (
                char == '"'
                and index + raw_hashes < len(chars)
                and chars[index + 1 : index + 1 + raw_hashes]
                == ["#"] * raw_hashes
            ):
                closing_hashes = raw_hashes
                raw_hashes = None
                index += 1 + closing_hashes
                continue
            index += 1
            continue
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            index += 1
            continue
        raw_prefix = raw_string_prefix(chars, index)
        if raw_prefix is not None:
            raw_hashes, opening_quote = raw_prefix
            index = opening_quote + 1
            continue
        if char == '"':
            quote = char
            index += 1
            continue
        if char == "'" and _looks_like_rust_char_literal(chars, index):
            quote = char
            index += 1
            continue
        if text.startswith("#[cfg(test)]", index):
            return index
        index += 1
    return None


def strip_rust_comments(text: str) -> str:
    """Strip Rust ``//`` and ``/* ... */`` comments without changing lines.

    This is intentionally a source filter, not a Rust lexer.  It only tracks
    quoted strings so comment markers inside the SQL literals that contain the
    production DDL remain intact.
    """

    chars = list(text)
    output: list[str] = []
    index = 0
    quote: str | None = None
    raw_hashes: int | None = None
    escaped = False
    block_comment_depth = 0
    while index < len(chars):
        char = chars[index]
        next_char = chars[index + 1] if index + 1 < len(chars) else ""
        if block_comment_depth:
            if char == "/" and next_char == "*":
                output.extend((" ", " "))
                index += 2
                block_comment_depth += 1
                continue
            if char == "*" and next_char == "/":
                output.extend((" ", " "))
                index += 2
                block_comment_depth -= 1
                continue
            output.append("\n" if char == "\n" else " ")
            index += 1
            continue
        if raw_hashes is not None:
            if char == '"' and index + raw_hashes < len(chars) and all(
                chars[index + offset] == "#" for offset in range(1, raw_hashes + 1)
            ):
                output.append(char)
                output.extend("#" for _ in range(raw_hashes))
                index += 1 + raw_hashes
                raw_hashes = None
            else:
                output.append(char)
                index += 1
            continue
        if quote is not None:
            output.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            index += 1
            continue
        raw_prefix = raw_string_prefix(chars, index)
        if raw_prefix is not None:
            raw_hashes, opening_quote = raw_prefix
            output.extend(chars[index : opening_quote + 1])
            index = opening_quote + 1
            continue
        if char == '"':
            quote = char
            output.append(char)
            index += 1
            continue
        if char == "'" and _looks_like_rust_char_literal(chars, index):
            quote = char
            output.append(char)
            index += 1
            continue
        if char == "/" and next_char == "/":
            output.extend((" ", " "))
            index += 2
            while index < len(chars) and chars[index] != "\n":
                output.append(" ")
                index += 1
            continue
        if char == "/" and next_char == "*":
            output.extend((" ", " "))
            index += 2
            block_comment_depth = 1
            continue
        output.append(char)
        index += 1
    return "".join(output)


def _looks_like_rust_char_literal(chars: list[str], index: int) -> bool:
    """Recognize a small Rust char literal without mistaking a lifetime for one."""

    next_index = index + 1
    if next_index >= len(chars) or chars[next_index] in {"\n", "\r"}:
        return False
    if chars[next_index] == "\\":
        return next_index + 2 < len(chars) and chars[next_index + 2] == "'"
    return next_index + 1 < len(chars) and chars[next_index + 1] == "'"


def find_matching_paren(text: str, open_index: int) -> int:
    depth = 0
    quote: str | None = None
    escaped = False
    line_comment = False
    block_comment = False
    for index in range(open_index, len(text)):
        char = text[index]
        next_char = text[index + 1] if index + 1 < len(text) else ""
        if line_comment:
            if char == "\n":
                line_comment = False
            continue
        if block_comment:
            if char == "*" and next_char == "/":
                block_comment = False
            continue
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {"'", '"', "`"}:
            quote = char
        elif char == "/" and next_char == "/":
            line_comment = True
        elif char == "/" and next_char == "*":
            block_comment = True
        elif char == "(":
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
    quote: str | None = None
    escaped = False
    for index, char in enumerate(body):
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {"'", '"', "`"}:
            quote = char
        elif char == "(":
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


def build_inventory(
    root: Path | None = None,
    *,
    sources: Iterable[SchemaSource] | None = None,
) -> dict[str, object]:
    root = root or REPO_ROOT
    manifest = tuple(SCHEMA_SOURCES if sources is None else sources)
    tables: list[TableInventory] = []
    for source in manifest:
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
        "schema_sources": [asdict(source) for source in manifest],
        "p1_5_consolidation_reviews": [
            asdict(review) for review in P1_5_CONSOLIDATION_REVIEWS
        ],
        "summary": {
            "source_count": len(manifest),
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
