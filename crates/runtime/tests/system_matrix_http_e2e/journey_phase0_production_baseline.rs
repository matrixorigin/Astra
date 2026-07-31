//! Live Server-only companion for the Phase-0 production history-work baseline.
//!
//! This is deliberately a fail-closed, ignored system journey. It accepts only
//! pre-existing Offering IDs, exercises the production CSL and `/chat/stream`
//! paths, and derives evidence from typed SSE events, production metrics, and
//! authoritative MatrixOne rows. It never installs bridge hooks, supplies mock
//! rounds, or calls history-work `record_*` helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use astra_core::history_work::{HistoryWorkScenario, HistoryWorkScenarioReport, HistoryWorkSite};
use astra_core::history_work_baseline::{
    AttemptTerminalStatus, BASELINE_RUN_ID_ENV, CacheEvidence, CompactionEvidence,
    CorrelationEvidence, EstimatorEvidence, FairnessEvidence, ModelMetadataSource,
    ModelOfferingEvidence, ProductionEntrypoint, ProductionProcessCapture, ProductionScenario,
    ProductionTopology, ProjectionEvidence, ProviderAttemptEvidence, ProviderUsageEvidence,
    ProviderUsageSource, ScenarioWorkEvidence, TenantAdmissionEvidence, WindowClass,
    scenario_capture_evidence, write_json_atomic,
};
use astra_turn_core::conversation_log::db_store::DbCslStore;
use astra_turn_core::conversation_log::manager::CslManager;
use astra_turn_core::conversation_log::{CslStore, SessionStateCompact};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{MySqlPool, Row};
use tower::util::ServiceExt;
use uuid::Uuid;

use super::harness::{
    BootstrapResult, MatrixE2eCtx, bootstrap, cleanup_session_data, model_selection, post_json,
};

const OFFERINGS_ENV: &str = "ASTRA_PHASE0_BASELINE_OFFERINGS_JSON";
const OUTPUT_DIR_ENV: &str = "ASTRA_PHASE0_BASELINE_DIR";
const EXCLUSIVE_ENV: &str = "ASTRA_PHASE0_BASELINE_EXCLUSIVE";
const HISTORY_TURNS: u32 = 6;
const STREAM_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROJECTION_TIMEOUT: Duration = Duration::from_secs(30);

const ADMISSION_ATTEMPTS_METRIC: &str = "astra_run_admission_attempts_total";
const ADMISSION_WAIT_MS_METRIC: &str = "astra_run_admission_wait_ms_total";
const ADMISSION_UNITS_METRIC: &str = "astra_run_admission_weight_units_total";

pub(super) type AnyError = Box<dyn Error + Send + Sync>;
pub(super) type AnyResult<T> = Result<T, AnyError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OfferingConfigDocument {
    offerings: Vec<OfferingConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OfferingConfig {
    window_class: WindowClass,
    offering_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedOffering {
    pub(super) window_class: WindowClass,
    pub(super) offering_id: String,
    pub(super) model_name: String,
    pub(super) provider: String,
    pub(super) context_window_tokens: u64,
}

#[derive(Debug)]
pub(super) struct LiveTenant {
    pub(super) user_id: String,
    pub(super) auth_header: String,
    pub(super) session_id: String,
}

#[derive(Debug)]
pub(super) struct StreamCapture {
    pub(super) session_id: String,
    pub(super) authority: StreamAuthority,
    pub(super) events: Vec<Value>,
}

#[derive(Debug)]
pub(super) enum StreamAuthority {
    DurableRun {
        run_id: String,
    },
    CliSessionBridge {
        execution_id: String,
        session_turn: u32,
        turn_chain_id: String,
        user_query_event_id: String,
        exchange_count: u32,
    },
}

impl StreamCapture {
    fn correlation_id(&self) -> &str {
        match &self.authority {
            StreamAuthority::DurableRun { run_id } => run_id,
            StreamAuthority::CliSessionBridge { turn_chain_id, .. } => turn_chain_id,
        }
    }

    pub(super) fn admitted_durable_requests(&self) -> u64 {
        match &self.authority {
            StreamAuthority::DurableRun { .. } => 1,
            StreamAuthority::CliSessionBridge { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct AdmissionCounters {
    pub(super) attempts: u64,
    pub(super) wait_ms: u64,
    pub(super) units: u64,
}

#[derive(Debug)]
struct ProviderAttemptRow {
    attempt_id: String,
    attempt_index: u32,
    round_index: u32,
    turn_index: u32,
    logical_attempt: u32,
    operation_id: String,
    wire_hash: String,
    wire_bytes: u64,
    status: AttemptTerminalStatus,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SseProviderAttemptAuthority {
    ExactSerializedProviderBodyV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SseProviderUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SseProviderCompositionBytes {
    system: u64,
    conversation: u64,
    tool_schema: u64,
    provider_envelope: u64,
    total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SseProviderCompositionItems {
    system: u64,
    conversation: u64,
    tool_schema: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SseProviderAttempt {
    authority: SseProviderAttemptAuthority,
    request_id: String,
    request_hash: String,
    round: u64,
    attempt: u64,
    protocol: String,
    provider_response_id: Option<String>,
    terminal_status: Option<AttemptTerminalStatus>,
    usage: Option<SseProviderUsage>,
    error_kind: Option<String>,
    error_message: Option<String>,
    serialized_bytes: u64,
    composition_bytes: SseProviderCompositionBytes,
    composition_items: SseProviderCompositionItems,
}

impl SseProviderAttempt {
    fn immutable_snapshot(&self) -> Self {
        let mut snapshot = self.clone();
        snapshot.provider_response_id = None;
        snapshot.terminal_status = None;
        snapshot.usage = None;
        snapshot.error_kind = None;
        snapshot.error_message = None;
        snapshot
    }

    fn validate_lifecycle(&self) -> AnyResult<()> {
        if self.request_id.is_empty() {
            return Err(invalid("SSE provider request_id cannot be empty"));
        }
        match self.terminal_status {
            None => {
                if self.provider_response_id.is_some()
                    || self.usage.is_some()
                    || self.error_kind.is_some()
                    || self.error_message.is_some()
                {
                    return Err(invalid(format!(
                        "SSE attempt {} has terminal facts before terminal_status",
                        self.request_id
                    )));
                }
            }
            Some(_) if self.usage.is_none() => {
                return Err(invalid(format!(
                    "SSE attempt {} has terminal_status without terminal usage",
                    self.request_id
                )));
            }
            Some(_) => {}
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RunEvidence {
    correlation: CorrelationEvidence,
    projection: ProjectionEvidence,
    estimator: EstimatorEvidence,
}

#[derive(Debug)]
struct CompletedOffering {
    window_class: WindowClass,
    scenario: ProductionScenario,
}

pub async fn run_server_only_production_baseline() {
    if let Err(error) = run_server_only_production_baseline_inner().await {
        panic!("Phase-0 ServerOnly production baseline failed closed: {error}");
    }
}

async fn run_server_only_production_baseline_inner() -> AnyResult<()> {
    require_exact_env(EXCLUSIVE_ENV, "1")?;
    require_exact_env("ASTRA_HISTORY_WORK_TRACE", "1")?;
    let configs = parse_offering_config(
        &std::env::var(OFFERINGS_ENV)
            .map_err(|_| invalid(format!("{OFFERINGS_ENV} must contain structured JSON")))?,
    )?;
    let baseline_run_id = required_baseline_run_id()?;
    let output_dir = required_output_dir()?;

    let bootstrap_result = bootstrap().await;
    let resolved = resolve_offerings(&bootstrap_result.ctx.pool, configs).await?;
    let mut completed = Vec::with_capacity(WindowClass::ALL.len());
    for offering in resolved {
        completed.push(run_offering(&bootstrap_result, &offering, &baseline_run_id).await?);
    }
    for result in completed {
        let stem = format!("server_only_{}", window_label(result.window_class));
        write_json_atomic(
            &output_dir.join(format!("{stem}.production_scenario.json")),
            &result.scenario,
        )?;
    }
    bootstrap_result.ctx.pool.close().await;
    Ok(())
}

async fn run_offering(
    bootstrap_result: &BootstrapResult,
    offering: &ResolvedOffering,
    baseline_run_id: &str,
) -> AnyResult<CompletedOffering> {
    let ctx = &bootstrap_result.ctx;
    let label = format!(
        "phase0-server-only-{}-{}",
        window_label(offering.window_class),
        Uuid::new_v4().simple()
    );
    let scenario_guard = HistoryWorkScenario::begin(label)?;

    let primary = create_session(
        &ctx.app,
        &bootstrap_result.auth_header,
        &ctx.user_id,
        offering,
        "primary",
    )
    .await?;
    seed_structured_history(ctx, &primary, offering).await?;

    let secondary = register_fairness_tenant(&ctx.app, offering).await?;

    let primary_before = scrape_admission_counters(&ctx.app).await?;
    let first = run_real_stream(&ctx.app, &primary, offering, "cold").await?;
    let second = run_real_stream(&ctx.app, &primary, offering, "warm").await?;
    let primary_after = scrape_admission_counters(&ctx.app).await?;
    let primary_fairness = tenant_admission_evidence(
        &primary.user_id,
        primary_before,
        primary_after,
        first
            .admitted_durable_requests()
            .checked_add(second.admitted_durable_requests())
            .ok_or_else(|| invalid("primary admitted-request count overflowed"))?,
    )?;

    let secondary_before = scrape_admission_counters(&ctx.app).await?;
    let secondary_stream = run_real_stream(&ctx.app, &secondary, offering, "fairness").await?;
    let secondary_after = scrape_admission_counters(&ctx.app).await?;
    let secondary_fairness = tenant_admission_evidence(
        &secondary.user_id,
        secondary_before,
        secondary_after,
        secondary_stream.admitted_durable_requests(),
    )?;

    let first_attempts =
        load_provider_attempts(&ctx.pool, &primary.user_id, &first, offering).await?;
    let second_attempts =
        load_provider_attempts(&ctx.pool, &primary.user_id, &second, offering).await?;
    let run_evidence =
        load_run_evidence(&ctx.pool, &primary, offering, &first, &first_attempts).await?;
    let mut main_attempts = first_attempts;
    main_attempts.extend(second_attempts);
    let distinct_attempts = main_attempts
        .iter()
        .map(|attempt| attempt.attempt_id.as_str())
        .collect::<BTreeSet<_>>();
    if distinct_attempts.len() != main_attempts.len() {
        return Err(invalid(
            "cold and warm-eligible runs returned duplicate provider attempts",
        ));
    }
    let secondary_attempts =
        load_provider_attempts(&ctx.pool, &secondary.user_id, &secondary_stream, offering).await?;
    if secondary_attempts.is_empty() {
        return Err(invalid("fairness tenant produced no provider attempt"));
    }

    let (provider_usage, cache) = provider_usage_and_cache(&main_attempts, &first, &second)?;
    let compaction = compaction_evidence(
        [
            (first.correlation_id(), first.events.as_slice()),
            (second.correlation_id(), second.events.as_slice()),
            (
                secondary_stream.correlation_id(),
                secondary_stream.events.as_slice(),
            ),
        ]
        .into_iter(),
    )?;

    let report = scenario_guard.finish()?;
    let work = work_evidence(&report)?;
    let fairness = FairnessEvidence {
        tenants: vec![primary_fairness, secondary_fairness],
    };
    let fairness_units = fairness.tenants.iter().try_fold(0_u64, |total, tenant| {
        total
            .checked_add(tenant.admission_units)
            .ok_or_else(|| invalid("fairness admission unit sum overflowed"))
    })?;
    if work.admission_units < fairness_units {
        return Err(invalid(format!(
            "history-work admission units {} are below exact per-owner metric delta {fairness_units}",
            work.admission_units
        )));
    }

    let scenario = ProductionScenario {
        baseline_run_id: baseline_run_id.to_string(),
        topology: ProductionTopology::ServerOnly,
        window_class: offering.window_class,
        // This in-process diagnostic has no production executable capture and
        // is intentionally ineligible for the external baseline artifact.
        capture_refs: Vec::new(),
        model: ModelOfferingEvidence {
            offering_id: offering.offering_id.clone(),
            resolved_model_name: offering.model_name.clone(),
            context_window_tokens: offering.context_window_tokens,
            metadata_source: ModelMetadataSource::DatabaseOffering,
        },
        entrypoints: BTreeSet::from([ProductionEntrypoint::ServerChatStream]),
        correlation: run_evidence.correlation,
        work,
        provider_usage,
        cache,
        projection: run_evidence.projection,
        compaction,
        estimator: run_evidence.estimator,
        fairness,
    };

    cleanup_session_data(&ctx.shared_pool, &primary.user_id, &primary.session_id).await;
    cleanup_session_data(&ctx.shared_pool, &secondary.user_id, &secondary.session_id).await;
    Ok(CompletedOffering {
        window_class: offering.window_class,
        scenario,
    })
}

/// Typed facts collected by the external production-topology orchestrator.
///
/// The orchestrator is responsible only for process lifecycle and transport.
/// This module keeps the DB/SSE/metrics reconciliation shared with the
/// ServerOnly companion so the three topologies cannot drift into distinct
/// evidence contracts.
pub(super) struct ExternalScenarioFacts {
    pub(super) baseline_run_id: String,
    pub(super) topology: ProductionTopology,
    pub(super) offering: ResolvedOffering,
    pub(super) primary: LiveTenant,
    pub(super) secondary: LiveTenant,
    pub(super) cold: StreamCapture,
    pub(super) warm_eligible: StreamCapture,
    pub(super) secondary_stream: StreamCapture,
    pub(super) primary_before: AdmissionCounters,
    pub(super) primary_after: AdmissionCounters,
    pub(super) secondary_before: AdmissionCounters,
    pub(super) secondary_after: AdmissionCounters,
    /// Exact successful primary-owner `/chat/stream` fairness-control calls
    /// made outside the measured cold/warm paths.
    pub(super) primary_fairness_control_requests: u64,
    pub(super) process_captures: Vec<ProductionProcessCapture>,
}

pub(super) async fn assemble_external_scenario(
    pool: &MySqlPool,
    facts: ExternalScenarioFacts,
) -> AnyResult<ProductionScenario> {
    let ExternalScenarioFacts {
        baseline_run_id,
        topology,
        offering,
        primary,
        secondary,
        cold,
        warm_eligible,
        secondary_stream,
        primary_before,
        primary_after,
        secondary_before,
        secondary_after,
        primary_fairness_control_requests,
        process_captures,
    } = facts;
    let capture_evidence = scenario_capture_evidence(
        &baseline_run_id,
        topology,
        offering.window_class,
        &process_captures,
    )?;
    let work = capture_evidence.work;

    let cold_attempts = load_provider_attempts(pool, &primary.user_id, &cold, &offering).await?;
    let warm_attempts =
        load_provider_attempts(pool, &primary.user_id, &warm_eligible, &offering).await?;
    let main_attempts = cold_attempts
        .iter()
        .chain(warm_attempts.iter())
        .collect::<Vec<_>>();
    let distinct_attempts = main_attempts
        .iter()
        .map(|attempt| attempt.attempt_id.as_str())
        .collect::<BTreeSet<_>>();
    if distinct_attempts.len() != main_attempts.len() {
        return Err(invalid(
            "cold and warm-eligible runs returned duplicate provider attempts",
        ));
    }
    let _secondary_attempts =
        load_provider_attempts(pool, &secondary.user_id, &secondary_stream, &offering).await?;

    let run_evidence = load_run_evidence(pool, &primary, &offering, &cold, &cold_attempts).await?;
    let (provider_usage, cache) =
        provider_usage_and_cache_refs(&main_attempts, &cold, &warm_eligible)?;
    let compaction = compaction_evidence(
        [
            (cold.correlation_id(), cold.events.as_slice()),
            (
                warm_eligible.correlation_id(),
                warm_eligible.events.as_slice(),
            ),
            (
                secondary_stream.correlation_id(),
                secondary_stream.events.as_slice(),
            ),
        ]
        .into_iter(),
    )?;

    let cold_warm_durable_requests = cold
        .admitted_durable_requests()
        .checked_add(warm_eligible.admitted_durable_requests())
        .ok_or_else(|| invalid("cold/warm admitted-request count overflowed"))?;
    match topology {
        ProductionTopology::CliServer if primary_fairness_control_requests != 1 => {
            return Err(invalid(
                "CLI topology requires exactly one typed primary fairness-control stream",
            ));
        }
        ProductionTopology::ServerOnly | ProductionTopology::EdgeServer
            if primary_fairness_control_requests != 0 =>
        {
            return Err(invalid(
                "durable cold/warm topology cannot add a primary fairness-control stream",
            ));
        }
        _ => {}
    }
    let primary_completed_requests = cold_warm_durable_requests
        .checked_add(primary_fairness_control_requests)
        .ok_or_else(|| invalid("primary admitted-request count overflowed"))?;
    let primary_fairness = tenant_admission_evidence(
        &primary.user_id,
        primary_before,
        primary_after,
        primary_completed_requests,
    )?;
    let secondary_fairness = tenant_admission_evidence(
        &secondary.user_id,
        secondary_before,
        secondary_after,
        secondary_stream.admitted_durable_requests(),
    )?;
    let fairness = FairnessEvidence {
        tenants: vec![primary_fairness, secondary_fairness],
    };
    let fairness_units = fairness.tenants.iter().try_fold(0_u64, |total, tenant| {
        total
            .checked_add(tenant.admission_units)
            .ok_or_else(|| invalid("fairness admission unit sum overflowed"))
    })?;
    if work.admission_units < fairness_units {
        return Err(invalid(format!(
            "history-work admission units {} are below exact per-owner metric delta {fairness_units}",
            work.admission_units
        )));
    }

    let entrypoints = match topology {
        ProductionTopology::CliServer => BTreeSet::from([
            ProductionEntrypoint::CliStreamTurn,
            ProductionEntrypoint::ServerChatStream,
        ]),
        ProductionTopology::ServerOnly => BTreeSet::from([ProductionEntrypoint::ServerChatStream]),
        ProductionTopology::EdgeServer => BTreeSet::from([
            ProductionEntrypoint::EdgeWebSocket,
            ProductionEntrypoint::ServerChatStream,
        ]),
    };

    Ok(ProductionScenario {
        baseline_run_id,
        topology,
        window_class: offering.window_class,
        capture_refs: capture_evidence.capture_refs,
        model: ModelOfferingEvidence {
            offering_id: offering.offering_id,
            resolved_model_name: offering.model_name,
            context_window_tokens: offering.context_window_tokens,
            metadata_source: ModelMetadataSource::DatabaseOffering,
        },
        entrypoints,
        correlation: run_evidence.correlation,
        work,
        provider_usage,
        cache,
        projection: run_evidence.projection,
        compaction,
        estimator: run_evidence.estimator,
        fairness,
    })
}

fn parse_offering_config(raw: &str) -> AnyResult<Vec<OfferingConfig>> {
    let document: OfferingConfigDocument = serde_json::from_str(raw)
        .map_err(|error| invalid(format!("invalid {OFFERINGS_ENV}: {error}")))?;
    if document.offerings.len() != WindowClass::ALL.len() {
        return Err(invalid(format!(
            "{OFFERINGS_ENV}.offerings must contain exactly three entries"
        )));
    }
    let mut by_class = BTreeMap::new();
    let mut offering_ids = BTreeSet::new();
    for mut entry in document.offerings {
        entry.offering_id = entry.offering_id.trim().to_string();
        if entry.offering_id.is_empty() {
            return Err(invalid("Offering ID cannot be empty"));
        }
        if !offering_ids.insert(entry.offering_id.clone()) {
            return Err(invalid(format!(
                "duplicate Offering ID: {}",
                entry.offering_id
            )));
        }
        let class = entry.window_class;
        if by_class.insert(class, entry).is_some() {
            return Err(invalid(format!(
                "duplicate window class: {}",
                window_label(class)
            )));
        }
    }
    WindowClass::ALL
        .into_iter()
        .map(|class| {
            by_class
                .remove(&class)
                .ok_or_else(|| invalid(format!("missing window class: {}", window_label(class))))
        })
        .collect()
}

async fn resolve_offerings(
    pool: &MySqlPool,
    configs: Vec<OfferingConfig>,
) -> AnyResult<Vec<ResolvedOffering>> {
    let mut resolved = Vec::with_capacity(configs.len());
    for config in configs {
        let row = sqlx::query(
            "SELECT model_name, provider, context_window, is_active \
             FROM infra_llm_models WHERE model_id = ? LIMIT 1",
        )
        .bind(&config.offering_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            invalid(format!(
                "configured Offering does not exist: {}",
                config.offering_id
            ))
        })?;
        let model_name: String = row.try_get("model_name")?;
        let provider: String = row.try_get("provider")?;
        let context_window = non_negative_i64(
            i64::from(row.try_get::<i32, _>("context_window")?),
            "infra_llm_models.context_window",
        )?;
        let is_active = row.try_get::<i16, _>("is_active")?;
        if is_active != 1 {
            return Err(invalid(format!(
                "Offering {} is not active",
                config.offering_id
            )));
        }
        if provider.trim().is_empty() || provider.eq_ignore_ascii_case("mock") {
            return Err(invalid(format!(
                "Offering {} must resolve to a real non-mock provider",
                config.offering_id
            )));
        }
        if model_name.trim().is_empty() {
            return Err(invalid(format!(
                "Offering {} has an empty resolved model name",
                config.offering_id
            )));
        }
        let expected = exact_context_window(config.window_class);
        if context_window != expected {
            return Err(invalid(format!(
                "Offering {} has context_window={context_window}; {} requires exactly {expected}",
                config.offering_id,
                window_label(config.window_class)
            )));
        }
        resolved.push(ResolvedOffering {
            window_class: config.window_class,
            offering_id: config.offering_id,
            model_name,
            provider,
            context_window_tokens: context_window,
        });
    }
    Ok(resolved)
}

async fn create_session(
    app: &Router,
    auth_header: &str,
    user_id: &str,
    offering: &ResolvedOffering,
    role: &str,
) -> AnyResult<LiveTenant> {
    let (status, body) = post_json(
        app,
        "/sessions",
        Some(auth_header),
        json!({
            "title": format!("Phase-0 {} {}", window_label(offering.window_class), role),
            "metadata": {
                "suite": "phase0_production_baseline",
                "topology": "server_only",
                "window_class": window_label(offering.window_class),
                "offering_id": offering.offering_id,
                "tenant_role": role,
            }
        }),
    )
    .await;
    if status != StatusCode::CREATED {
        return Err(invalid(format!(
            "create {role} session returned {status}: {body}"
        )));
    }
    let session_id = required_string(&body, "session_id", "session create response")?;
    Ok(LiveTenant {
        user_id: user_id.to_string(),
        auth_header: auth_header.to_string(),
        session_id,
    })
}

async fn register_fairness_tenant(
    app: &Router,
    offering: &ResolvedOffering,
) -> AnyResult<LiveTenant> {
    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("phase0_fairness_{suffix}");
    let (status, body) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": username,
            "email": format!("phase0_fairness_{suffix}@e2e.test"),
            "password": super::harness::E2E_PASSWORD,
            "display_name": "Phase-0 fairness tenant"
        }),
    )
    .await;
    if status != StatusCode::CREATED {
        return Err(invalid(format!(
            "register fairness tenant returned {status}: {body}"
        )));
    }
    let user_id = required_string(&body, "user_id", "fairness register response")?;
    let access = required_string(&body, "access_token", "fairness register response")?;
    create_session(
        app,
        &format!("Bearer {access}"),
        &user_id,
        offering,
        "fairness",
    )
    .await
}

async fn seed_structured_history(
    ctx: &MatrixE2eCtx,
    tenant: &LiveTenant,
    offering: &ResolvedOffering,
) -> AnyResult<()> {
    seed_structured_history_with_pool(&ctx.shared_pool, tenant, offering).await
}

pub(super) async fn seed_structured_history_with_pool(
    shared_pool: &astra_core::SharedPool,
    tenant: &LiveTenant,
    offering: &ResolvedOffering,
) -> AnyResult<()> {
    let store: Arc<dyn CslStore> = Arc::new(
        DbCslStore::new(shared_pool.settings().clone(), tenant.user_id.clone())?
            .with_pool(shared_pool.clone()),
    );
    let mut manager = CslManager::new(store, tenant.session_id.clone(), Default::default())?;
    let target_tokens = offering.context_window_tokens.saturating_mul(78) / 100;
    let unit = "durable_fact { key: phase0_history, value: preserved_across_turns };\n";
    let unit_tokens = u64::from(astra_turn_core::section_types::estimate_text_tokens(unit)).max(1);
    let message_count = u64::from(HISTORY_TURNS) * 2;
    let repeats_per_message = target_tokens
        .div_ceil(unit_tokens.saturating_mul(message_count))
        .max(1);
    let repeats = usize::try_from(repeats_per_message)
        .map_err(|_| invalid("history repeat count exceeds usize"))?;
    let bulk = unit.repeat(repeats);
    let mut messages = Vec::with_capacity(message_count as usize);
    for turn in 1..=HISTORY_TURNS {
        messages.push(json!({
            "role": "user",
            "content": format!("turn={turn}; request=retain typed production history\n{bulk}"),
            "metadata": {
                "schema": "astra.phase0.structured_history.v1",
                "turn": turn,
                "kind": "user_requirement"
            }
        }));
        messages.push(json!({
            "role": "assistant",
            "content": format!("turn={turn}; acknowledgement=durably retained\n{bulk}"),
            "metadata": {
                "schema": "astra.phase0.structured_history.v1",
                "turn": turn,
                "kind": "assistant_checkpoint"
            }
        }));
        manager
            .persist_turn(turn, &messages, &SessionStateCompact::default())
            .await?;
    }
    Ok(())
}

async fn run_real_stream(
    app: &Router,
    tenant: &LiveTenant,
    offering: &ResolvedOffering,
    phase: &str,
) -> AnyResult<StreamCapture> {
    let payload = json!({
        "message": format!(
            "Phase-0 production baseline phase={phase}. Reply concisely without calling tools."
        ),
        "session_id": tenant.session_id,
        "model_selection": model_selection(offering.offering_id.clone()),
        "context": {
            "phase0_production_baseline": {
                "schema": "astra.phase0.server_only_request.v1",
                "phase": phase,
                "window_class": window_label(offering.window_class)
            }
        }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", &tenant.auth_header)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))?;
    let response = app.clone().oneshot(request).await?;
    let status = response.status();
    let mut body = response.into_body().into_data_stream();
    let deadline = tokio::time::Instant::now() + STREAM_TIMEOUT;
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, body.next()).await {
            Ok(Some(Ok(chunk))) => bytes.extend_from_slice(&chunk),
            Ok(Some(Err(error))) => {
                return Err(invalid(format!("chat/stream body error: {error}")));
            }
            Ok(None) => break,
            Err(_) => {
                return Err(invalid(format!(
                    "chat/stream exceeded {:?} for {}",
                    STREAM_TIMEOUT, offering.offering_id
                )));
            }
        }
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| invalid(format!("chat/stream returned non-UTF8 SSE: {error}")))?;
    if !status.is_success() {
        return Err(invalid(format!(
            "real chat/stream returned {status}: {}",
            text.chars().take(2_000).collect::<String>()
        )));
    }
    let events = parse_sse_events(&text)?;
    if let Some(event) = events
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("error"))
    {
        return Err(invalid(format!("chat/stream emitted typed error: {event}")));
    }
    let session_info = events
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("session_info"))
        .ok_or_else(|| invalid("chat/stream omitted typed session_info"))?;
    let session_id = required_string(session_info, "session_id", "session_info")?;
    let run_id = required_string(session_info, "run_id", "session_info")?;
    if session_id != tenant.session_id {
        return Err(invalid(format!(
            "session_info session {} != requested {}",
            session_id, tenant.session_id
        )));
    }
    Ok(StreamCapture {
        session_id,
        authority: StreamAuthority::DurableRun { run_id },
        events,
    })
}

pub(super) fn parse_sse_events(raw: &str) -> AnyResult<Vec<Value>> {
    let mut events = Vec::new();
    for (line_index, line) in raw.lines().enumerate() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            return Err(invalid(format!(
                "empty SSE data field at line {}",
                line_index + 1
            )));
        }
        let event: Value = serde_json::from_str(data).map_err(|error| {
            invalid(format!(
                "malformed SSE JSON at line {}: {error}",
                line_index + 1
            ))
        })?;
        if !event.is_object() || event.get("type").and_then(Value::as_str).is_none() {
            return Err(invalid(format!(
                "SSE data at line {} is not a typed object",
                line_index + 1
            )));
        }
        events.push(event);
    }
    if events.is_empty() {
        return Err(invalid("chat/stream returned no typed SSE events"));
    }
    Ok(events)
}

async fn load_provider_attempts(
    pool: &MySqlPool,
    user_id: &str,
    stream: &StreamCapture,
    offering: &ResolvedOffering,
) -> AnyResult<Vec<ProviderAttemptRow>> {
    let typed_attempts = typed_sse_attempts(&stream.events, stream.correlation_id())?;
    let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT a.attempt_id, a.attempt_index, a.provider, a.provider_wire_hash, \
                a.provider_wire_bytes, a.status, a.input_tokens, a.output_tokens, \
                a.cache_read_tokens, a.cache_creation_tokens, a.session_id AS attempt_session_id, \
                a.run_id AS attempt_run_id, i.session_id AS invocation_session_id, \
                i.scope_kind AS invocation_scope_kind, i.run_id AS invocation_run_id, \
                i.turn_index, i.round_index, i.logical_attempt, i.operation_id, i.purpose, \
                r.session_id AS route_session_id, r.scope_kind AS route_scope_kind, \
                r.run_id AS route_run_id, r.offering_id, r.resolved_model_name \
         FROM inference_provider_attempts a \
         INNER JOIN inference_invocations i \
           ON i.user_id = a.user_id AND i.invocation_id = a.invocation_id \
         INNER JOIN inference_routes r \
           ON r.user_id = i.user_id AND r.route_id = i.route_id \
         WHERE a.user_id = ",
    );
    query.push_bind(user_id);
    query.push(" AND a.attempt_id IN (");
    {
        let mut separated = query.separated(", ");
        for attempt_id in typed_attempts.keys() {
            separated.push_bind(attempt_id);
        }
    }
    query.push(
        ") ORDER BY i.turn_index, i.round_index, i.logical_attempt, \
         a.attempt_index, a.attempt_id",
    );
    let rows = query.build().fetch_all(pool).await?;
    if rows.is_empty() {
        return Err(invalid(format!(
            "no authoritative provider attempts for exact trace IDs on {user_id}/{}",
            stream.correlation_id()
        )));
    }
    let mut attempts = Vec::with_capacity(rows.len());
    let mut observed_ids = BTreeSet::new();
    for row in rows {
        let attempt_id = required_nonempty(row.try_get::<String, _>("attempt_id")?, "attempt_id")?;
        if !observed_ids.insert(attempt_id.clone()) {
            return Err(invalid(format!(
                "exact attempt-id join duplicated {attempt_id}"
            )));
        }
        let route_offering: String = row.try_get("offering_id")?;
        let route_model: String = row.try_get("resolved_model_name")?;
        if route_offering != offering.offering_id || route_model != offering.model_name {
            return Err(invalid(format!(
                "inference route resolved {route_offering}/{route_model}, expected {}/{}",
                offering.offering_id, offering.model_name
            )));
        }
        let provider: String = row.try_get("provider")?;
        if provider != offering.provider {
            return Err(invalid(format!(
                "provider attempt used {provider}, expected {}",
                offering.provider
            )));
        }
        let attempt_session_id: Option<String> = row.try_get("attempt_session_id")?;
        let invocation_session_id: Option<String> = row.try_get("invocation_session_id")?;
        let route_session_id: Option<String> = row.try_get("route_session_id")?;
        if attempt_session_id.as_deref() != Some(stream.session_id.as_str())
            || invocation_session_id.as_deref() != Some(stream.session_id.as_str())
            || route_session_id.as_deref() != Some(stream.session_id.as_str())
        {
            return Err(invalid(format!(
                "attempt {attempt_id} owner/session join does not match {user_id}/{}",
                stream.session_id
            )));
        }
        let attempt_run_id: Option<String> = row.try_get("attempt_run_id")?;
        let invocation_run_id: Option<String> = row.try_get("invocation_run_id")?;
        let route_run_id: Option<String> = row.try_get("route_run_id")?;
        let invocation_scope: String = row.try_get("invocation_scope_kind")?;
        let route_scope: String = row.try_get("route_scope_kind")?;
        let turn_index = non_negative_u32(
            required_i64(row.try_get("turn_index")?, "turn_index")?,
            "turn_index",
        )?;
        let round_index = non_negative_u32(
            required_i64(row.try_get("round_index")?, "round_index")?,
            "round_index",
        )?;
        let logical_attempt = non_negative_u32(row.try_get("logical_attempt")?, "logical_attempt")?;
        let operation_id =
            required_nonempty(row.try_get::<String, _>("operation_id")?, "operation_id")?;
        let purpose = required_nonempty(row.try_get::<String, _>("purpose")?, "purpose")?;
        match &stream.authority {
            StreamAuthority::DurableRun { run_id } => {
                if invocation_scope != "run"
                    || route_scope != "run"
                    || attempt_run_id.as_deref() != Some(run_id)
                    || invocation_run_id.as_deref() != Some(run_id)
                    || route_run_id.as_deref() != Some(run_id)
                {
                    return Err(invalid(format!(
                        "attempt {attempt_id} is not owned by exact durable run {run_id}"
                    )));
                }
            }
            StreamAuthority::CliSessionBridge {
                session_turn,
                user_query_event_id,
                ..
            } => {
                if invocation_scope != "session"
                    || route_scope != "session"
                    || attempt_run_id.is_some()
                    || invocation_run_id.is_some()
                    || route_run_id.is_some()
                {
                    return Err(invalid(format!(
                        "CLI attempt {attempt_id} fabricated durable run ownership"
                    )));
                }
                if turn_index != *session_turn {
                    return Err(invalid(format!(
                        "CLI attempt {attempt_id} turn {turn_index} != stream session_turn {session_turn}"
                    )));
                }
                if purpose != "primary_agent" {
                    return Err(invalid(format!(
                        "CLI primary context trace contains inference purpose {purpose}"
                    )));
                }
                let expected_operation = bridge_inference_operation_id(user_query_event_id);
                if operation_id != expected_operation {
                    return Err(invalid(format!(
                        "CLI attempt {attempt_id} operation_id {operation_id} != {expected_operation}"
                    )));
                }
            }
        }
        let wire_hash: String = row.try_get("provider_wire_hash")?;
        if wire_hash.len() != 64
            || !wire_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid("provider wire hash is not lowercase SHA-256"));
        }
        let status_text: String = row.try_get("status")?;
        let status = match status_text.as_str() {
            "succeeded" => AttemptTerminalStatus::Succeeded,
            "failed" => AttemptTerminalStatus::Failed,
            "delivery_unknown" => AttemptTerminalStatus::DeliveryUnknown,
            other => {
                return Err(invalid(format!(
                    "provider attempt has non-terminal/unsupported status {other}"
                )));
            }
        };
        attempts.push(ProviderAttemptRow {
            attempt_id,
            attempt_index: non_negative_u32(row.try_get("attempt_index")?, "attempt_index")?,
            round_index,
            turn_index,
            logical_attempt,
            operation_id,
            wire_hash,
            wire_bytes: positive_i64(row.try_get("provider_wire_bytes")?, "wire bytes")?,
            status,
            input_tokens: non_negative_i64(row.try_get("input_tokens")?, "input_tokens")?,
            output_tokens: non_negative_i64(row.try_get("output_tokens")?, "output_tokens")?,
            cache_read_tokens: non_negative_i64(
                row.try_get("cache_read_tokens")?,
                "cache_read_tokens",
            )?,
            cache_creation_tokens: non_negative_i64(
                row.try_get("cache_creation_tokens")?,
                "cache_creation_tokens",
            )?,
        });
    }
    let expected_ids = typed_attempts.keys().cloned().collect::<BTreeSet<_>>();
    if observed_ids != expected_ids {
        return Err(invalid(format!(
            "exact provider attempt-id join is incomplete: expected={expected_ids:?}, observed={observed_ids:?}"
        )));
    }
    cross_check_sse_attempts(&stream.events, &attempts, stream.correlation_id())?;
    Ok(attempts)
}

async fn load_run_evidence(
    pool: &MySqlPool,
    tenant: &LiveTenant,
    offering: &ResolvedOffering,
    stream: &StreamCapture,
    attempts: &[ProviderAttemptRow],
) -> AnyResult<RunEvidence> {
    if stream.session_id != tenant.session_id {
        return Err(invalid(
            "stream/session correlation changed before DB query",
        ));
    }
    if attempts.is_empty() {
        return Err(invalid(
            "representative stream has no exact physical provider attempts",
        ));
    }
    let turn = attempts[0].turn_index;
    if turn == 0 {
        return Err(invalid(
            "authoritative inference invocation turn_index is zero; expected the 1-based production session turn",
        ));
    }
    if attempts.iter().any(|attempt| attempt.turn_index != turn) {
        return Err(invalid(
            "representative run attempts span multiple turn indices",
        ));
    }
    let provider_attempts = attempts
        .iter()
        .map(|attempt| ProviderAttemptEvidence {
            request_id: attempt.attempt_id.clone(),
            round: attempt.round_index,
            logical_attempt: attempt.logical_attempt,
            attempt: attempt.attempt_index,
            operation_id: attempt.operation_id.clone(),
            wire_request_sha256: attempt.wire_hash.clone(),
            wire_request_bytes: attempt.wire_bytes,
            terminal_status: attempt.status,
        })
        .collect::<Vec<_>>();
    let estimated_input_tokens = sse_estimated_input_tokens(&stream.events)?;
    let canonical_provider_input_tokens = attempts
        .iter()
        .filter(|attempt| attempt.status == AttemptTerminalStatus::Succeeded)
        .try_fold(0_u64, |total, attempt| {
            attempt
                .input_tokens
                .checked_add(attempt.cache_read_tokens)
                .and_then(|value| value.checked_add(attempt.cache_creation_tokens))
                .and_then(|value| total.checked_add(value))
                .ok_or_else(|| invalid("canonical provider input sum overflowed"))
        })?;
    if canonical_provider_input_tokens == 0 {
        return Err(invalid(
            "successful representative attempts have no canonical provider input usage",
        ));
    }

    let (correlation, projection) = match &stream.authority {
        StreamAuthority::DurableRun { run_id } => {
            let run = wait_for_projection(pool, &tenant.user_id, run_id).await?;
            let run_status: String = run.try_get("run_status")?;
            let projection_status: String = run.try_get("projection_status")?;
            if run_status != "completed" || projection_status != run_status {
                return Err(invalid(format!(
                    "run/projection status mismatch: run={run_status}, projection={projection_status}"
                )));
            }
            let run_offering: String = run.try_get("model_offering_id")?;
            let run_model: String = run.try_get("resolved_model_name")?;
            if run_offering != offering.offering_id || run_model != offering.model_name {
                return Err(invalid(format!(
                    "agent_runs Offering/model mismatch: {run_offering}/{run_model}"
                )));
            }
            let durable = non_negative_i64(run.try_get("last_event_idx")?, "last_event_idx")?;
            let projected =
                non_negative_i64(run.try_get("projection_event_idx")?, "projection_event_idx")?;
            if durable == 0 || projected == 0 {
                return Err(invalid(format!(
                    "run projection cursors must be positive: durable={durable}, projected={projected}"
                )));
            }
            if projected > durable {
                return Err(invalid("run projection is ahead of durable event index"));
            }
            (
                CorrelationEvidence::DurableRun {
                    owner_id: tenant.user_id.clone(),
                    session_id: tenant.session_id.clone(),
                    run_id: run_id.clone(),
                    turn,
                    provider_attempts,
                },
                ProjectionEvidence::DurableRun {
                    durable_event_index: durable,
                    projected_event_index: projected,
                    lag_events: durable - projected,
                },
            )
        }
        StreamAuthority::CliSessionBridge {
            execution_id,
            session_turn,
            turn_chain_id,
            user_query_event_id,
            exchange_count,
        } => {
            if turn != *session_turn {
                return Err(invalid(format!(
                    "CLI representative turn {turn} != stream session_turn {session_turn}"
                )));
            }
            verify_cli_non_durable_authority(pool, &tenant.user_id, execution_id, turn_chain_id)
                .await?;
            (
                CorrelationEvidence::CliSessionBridge {
                    owner_id: tenant.user_id.clone(),
                    session_id: tenant.session_id.clone(),
                    cli_execution_id: execution_id.clone(),
                    session_turn: *session_turn,
                    turn_chain_id: turn_chain_id.clone(),
                    user_query_event_id: user_query_event_id.clone(),
                    exchange_count: *exchange_count,
                    provider_attempts,
                },
                ProjectionEvidence::CliSessionBridgeNotApplicable,
            )
        }
    };

    Ok(RunEvidence {
        correlation,
        projection,
        estimator: EstimatorEvidence {
            estimated_input_tokens,
            canonical_provider_input_tokens,
            absolute_error_tokens: estimated_input_tokens.abs_diff(canonical_provider_input_tokens),
        },
    })
}

async fn verify_cli_non_durable_authority(
    pool: &MySqlPool,
    user_id: &str,
    execution_id: &str,
    turn_chain_id: &str,
) -> AnyResult<()> {
    for table in ["agent_runs", "run_display_projections"] {
        let query = format!(
            "SELECT COUNT(*) AS row_count FROM {table} \
             WHERE user_id = ? AND (run_id = ? OR run_id = ?)"
        );
        let row = sqlx::query(&query)
            .bind(user_id)
            .bind(execution_id)
            .bind(turn_chain_id)
            .fetch_one(pool)
            .await?;
        let row_count = non_negative_i64(row.try_get("row_count")?, "row_count")?;
        if row_count != 0 {
            return Err(invalid(format!(
                "CLI session bridge unexpectedly owns {row_count} exact {table} rows"
            )));
        }
    }
    Ok(())
}

fn bridge_inference_operation_id(user_query_event_id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(user_query_event_id.as_bytes()));
    format!("bridge_chat_{}", &digest[..32])
}

async fn wait_for_projection(
    pool: &MySqlPool,
    user_id: &str,
    run_id: &str,
) -> AnyResult<sqlx::mysql::MySqlRow> {
    let deadline = tokio::time::Instant::now() + PROJECTION_TIMEOUT;
    loop {
        let row = sqlx::query(
            "SELECT r.status AS run_status, r.last_event_idx, r.model_offering_id, \
                    r.resolved_model_name, p.status AS projection_status, p.projection_event_idx \
             FROM agent_runs r \
             INNER JOIN run_display_projections p \
               ON p.user_id = r.user_id AND p.run_id = r.run_id \
             WHERE r.user_id = ? AND r.run_id = ? LIMIT 1",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(pool)
        .await?;
        if let Some(row) = row {
            let durable: i64 = row.try_get("last_event_idx")?;
            let projected: i64 = row.try_get("projection_event_idx")?;
            let run_status: String = row.try_get("run_status")?;
            let projection_status: String = row.try_get("projection_status")?;
            if durable >= 0
                && projected == durable
                && run_status == "completed"
                && projection_status == run_status
            {
                return Ok(row);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(invalid(format!(
                "authoritative run projection did not converge within {:?}: {run_id}",
                PROJECTION_TIMEOUT
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn cross_check_sse_attempts(
    events: &[Value],
    db_attempts: &[ProviderAttemptRow],
    run_id: &str,
) -> AnyResult<()> {
    let sse_attempts = typed_sse_attempts(events, run_id)?;
    let sse_attempt_ids = sse_attempts.keys().cloned().collect::<BTreeSet<_>>();
    let db_attempt_ids = db_attempts
        .iter()
        .map(|attempt| attempt.attempt_id.clone())
        .collect::<BTreeSet<_>>();
    if db_attempt_ids.len() != db_attempts.len() {
        return Err(invalid(format!(
            "run {run_id} has duplicate authoritative DB provider attempt ids"
        )));
    }
    if sse_attempt_ids != db_attempt_ids {
        let missing_from_sse = db_attempt_ids
            .difference(&sse_attempt_ids)
            .cloned()
            .collect::<Vec<_>>();
        let missing_from_db = sse_attempt_ids
            .difference(&db_attempt_ids)
            .cloned()
            .collect::<Vec<_>>();
        return Err(invalid(format!(
            "run {run_id} SSE/DB provider attempt-id sets differ: \
             missing_from_sse={missing_from_sse:?}, missing_from_db={missing_from_db:?}"
        )));
    }
    for (request_id, item) in sse_attempts {
        let attempt = db_attempts
            .iter()
            .find(|attempt| attempt.attempt_id == request_id)
            .ok_or_else(|| {
                invalid(format!(
                    "SSE provider request {request_id} has no authoritative DB row"
                ))
            })?;
        if item.request_hash != attempt.wire_hash
            || item.serialized_bytes != attempt.wire_bytes
            || item.round != u64::from(attempt.round_index)
            || item.attempt != u64::from(attempt.attempt_index)
        {
            return Err(invalid(format!(
                "SSE/DB provider request identity mismatch for {request_id}"
            )));
        }
        if item.terminal_status != Some(attempt.status) {
            return Err(invalid(format!(
                "SSE/DB terminal mismatch for {request_id}: {:?} != {:?}",
                item.terminal_status, attempt.status
            )));
        }
    }
    Ok(())
}

fn typed_sse_attempts(
    events: &[Value],
    run_id: &str,
) -> AnyResult<BTreeMap<String, SseProviderAttempt>> {
    let mut sse_attempts = BTreeMap::<String, SseProviderAttempt>::new();
    for event in events {
        let Some(items) = event
            .pointer("/context_manifest_trace/provider_request_attempts")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for item in items {
            let observed =
                serde_json::from_value::<SseProviderAttempt>(item.clone()).map_err(|error| {
                    invalid(format!(
                        "invalid typed SSE provider_request_attempt: {error}"
                    ))
                })?;
            observed.validate_lifecycle()?;
            let request_id = observed.request_id.clone();
            if let Some(previous) = sse_attempts.get(&request_id) {
                if previous.immutable_snapshot() != observed.immutable_snapshot() {
                    return Err(invalid(format!(
                        "SSE attempt {request_id} changed immutable request identity"
                    )));
                }
                match (previous.terminal_status, observed.terminal_status) {
                    (None, _) => {}
                    (Some(_), None) => {
                        return Err(invalid(format!(
                            "SSE attempt {request_id} regressed from terminal to admitted"
                        )));
                    }
                    (Some(_), Some(_)) if previous == &observed => continue,
                    (Some(_), Some(_)) => {
                        return Err(invalid(format!(
                            "SSE attempt {request_id} changed after reaching terminal state"
                        )));
                    }
                }
            }
            sse_attempts.insert(request_id, observed);
        }
    }
    if sse_attempts.is_empty() {
        return Err(invalid(format!(
            "run {run_id} emitted no exact provider_request_attempts trace"
        )));
    }
    Ok(sse_attempts)
}

fn provider_usage_and_cache(
    attempts: &[ProviderAttemptRow],
    cold_stream: &StreamCapture,
    warm_eligible_stream: &StreamCapture,
) -> AnyResult<(ProviderUsageEvidence, CacheEvidence)> {
    let attempts = attempts.iter().collect::<Vec<_>>();
    provider_usage_and_cache_refs(&attempts, cold_stream, warm_eligible_stream)
}

fn provider_usage_and_cache_refs(
    attempts: &[&ProviderAttemptRow],
    cold_stream: &StreamCapture,
    warm_eligible_stream: &StreamCapture,
) -> AnyResult<(ProviderUsageEvidence, CacheEvidence)> {
    let cold_ids = typed_sse_attempts(&cold_stream.events, cold_stream.correlation_id())?
        .into_keys()
        .collect::<BTreeSet<_>>();
    let warm_eligible_ids = typed_sse_attempts(
        &warm_eligible_stream.events,
        warm_eligible_stream.correlation_id(),
    )?
    .into_keys()
    .collect::<BTreeSet<_>>();
    provider_usage_and_cache_for_partitions(attempts, &cold_ids, &warm_eligible_ids)
}

fn provider_usage_and_cache_for_partitions(
    attempts: &[&ProviderAttemptRow],
    cold_ids: &BTreeSet<String>,
    warm_eligible_ids: &BTreeSet<String>,
) -> AnyResult<(ProviderUsageEvidence, CacheEvidence)> {
    if !cold_ids.is_disjoint(warm_eligible_ids) {
        return Err(invalid(
            "cold and warm-eligible paths share a physical provider attempt",
        ));
    }
    for attempt in attempts {
        match (
            cold_ids.contains(&attempt.attempt_id),
            warm_eligible_ids.contains(&attempt.attempt_id),
        ) {
            (true, false) | (false, true) => {}
            (false, false) => {
                return Err(invalid(format!(
                    "DB provider attempt {} is absent from both cold/warm measured partitions",
                    attempt.attempt_id
                )));
            }
            (true, true) => {
                return Err(invalid(format!(
                    "DB provider attempt {} appears in both cold/warm measured partitions",
                    attempt.attempt_id
                )));
            }
        }
    }
    let successful: Vec<_> = attempts
        .iter()
        .filter(|attempt| attempt.status == AttemptTerminalStatus::Succeeded)
        .collect();
    if successful.is_empty() {
        return Err(invalid("no successful real provider attempts"));
    }
    let mut fresh = 0_u64;
    let mut cache_read = 0_u64;
    let mut cache_creation = 0_u64;
    let mut output = 0_u64;
    let mut cold_path = 0_u64;
    let mut warm_eligible_path = 0_u64;
    let mut observed_cache_read = 0_u64;
    for attempt in successful {
        let normalized = attempt
            .input_tokens
            .checked_add(attempt.cache_read_tokens)
            .and_then(|value| value.checked_add(attempt.cache_creation_tokens))
            .ok_or_else(|| invalid("provider input usage overflowed"))?;
        if normalized == 0 || attempt.output_tokens == 0 {
            return Err(invalid(format!(
                "provider omitted canonical input/output metrics for {}",
                attempt.attempt_id
            )));
        }
        fresh = checked_add(fresh, attempt.input_tokens, "fresh input")?;
        cache_read = checked_add(cache_read, attempt.cache_read_tokens, "cache read")?;
        cache_creation = checked_add(
            cache_creation,
            attempt.cache_creation_tokens,
            "cache creation",
        )?;
        output = checked_add(output, attempt.output_tokens, "output")?;
        match (
            cold_ids.contains(&attempt.attempt_id),
            warm_eligible_ids.contains(&attempt.attempt_id),
        ) {
            (true, false) => cold_path = checked_add(cold_path, 1, "cold-path requests")?,
            (false, true) => {
                warm_eligible_path =
                    checked_add(warm_eligible_path, 1, "warm-eligible-path requests")?;
            }
            (false, false) => {
                return Err(invalid(format!(
                    "successful provider attempt {} belongs to neither measured cache path",
                    attempt.attempt_id
                )));
            }
            (true, true) => {
                return Err(invalid(format!(
                    "provider attempt {} belongs to both measured cache paths",
                    attempt.attempt_id
                )));
            }
        }
        if attempt.cache_read_tokens > 0 {
            observed_cache_read =
                checked_add(observed_cache_read, 1, "observed cache-read requests")?;
        }
    }
    if cold_path == 0 || warm_eligible_path == 0 {
        return Err(invalid(format!(
            "provider cache path evidence is incomplete: cold={cold_path}, \
             warm_eligible={warm_eligible_path}"
        )));
    }
    let normalized = fresh
        .checked_add(cache_read)
        .and_then(|value| value.checked_add(cache_creation))
        .ok_or_else(|| invalid("normalized provider usage overflowed"))?;
    Ok((
        ProviderUsageEvidence {
            source: ProviderUsageSource::ProviderResponse,
            requests: cold_path + warm_eligible_path,
            fresh_input_tokens: fresh,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            normalized_input_tokens: normalized,
            output_tokens: output,
        },
        CacheEvidence {
            cold_path_requests: cold_path,
            warm_eligible_path_requests: warm_eligible_path,
            observed_cache_read_requests: observed_cache_read,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            total_input_tokens: normalized,
        },
    ))
}

fn compaction_evidence<'a>(
    streams: impl Iterator<Item = (&'a str, &'a [Value])>,
) -> AnyResult<CompactionEvidence> {
    let mut typed = BTreeMap::<(String, String), (u64, u64)>::new();
    let mut standalone = Vec::new();
    for (stream_id, events) in streams {
        for event in events {
            if event.get("type").and_then(Value::as_str) == Some("context_meta")
                && let Some(compactions) = event.get("compactions").and_then(Value::as_array)
            {
                for item in compactions {
                    let id = required_string(item, "id", "context_meta compaction")?;
                    let before = required_u64(item, "tokens_before", "context_meta compaction")?;
                    let after = required_u64(item, "tokens_after", "context_meta compaction")?;
                    let saved = required_u64(item, "tokens_saved", "context_meta compaction")?;
                    if before < after || saved != before - after {
                        return Err(invalid(format!(
                            "inconsistent typed context compaction {stream_id}/{id}"
                        )));
                    }
                    let scoped_id = (stream_id.to_string(), id);
                    if let Some(previous) = typed.insert(scoped_id.clone(), (before, after))
                        && previous != (before, after)
                    {
                        return Err(invalid(format!(
                            "typed context compaction {}/{} changed across SSE snapshots",
                            scoped_id.0, scoped_id.1
                        )));
                    }
                }
            }
            if event.get("type").and_then(Value::as_str) == Some("compaction")
                && let Some(data) = event.get("data")
            {
                let before = required_u64(data, "tokens_before", "compaction event")?;
                let after = required_u64(data, "tokens_after", "compaction event")?;
                let freed = required_u64(data, "tokens_freed", "compaction event")?;
                if before < after || freed != before - after {
                    return Err(invalid("inconsistent standalone compaction event"));
                }
                standalone.push((before, after));
            }
        }
    }
    let facts: Vec<_> = if typed.is_empty() {
        standalone
    } else {
        typed.into_values().collect()
    };
    if facts.is_empty() {
        // A zero rate is still measured evidence. Topology-aware validation
        // decides whether the scenario was required to create pressure.
        return Ok(CompactionEvidence {
            attempts: 0,
            effective_attempts: 0,
            input_tokens: 0,
            output_tokens: 0,
            tokens_freed: 0,
        });
    }
    let attempts =
        u64::try_from(facts.len()).map_err(|_| invalid("compaction count overflowed"))?;
    let effective_attempts = u64::try_from(
        facts
            .iter()
            .filter(|(before, after)| before > after)
            .count(),
    )
    .map_err(|_| invalid("effective compaction count overflowed"))?;
    if effective_attempts == 0 {
        return Err(invalid("all observed compaction attempts were ineffective"));
    }
    let (input_tokens, output_tokens) =
        facts
            .into_iter()
            .try_fold((0_u64, 0_u64), |(input, output), (before, after)| {
                Ok::<_, AnyError>((
                    checked_add(input, before, "compaction input")?,
                    checked_add(output, after, "compaction output")?,
                ))
            })?;
    Ok(CompactionEvidence {
        attempts,
        effective_attempts,
        input_tokens,
        output_tokens,
        tokens_freed: input_tokens - output_tokens,
    })
}

fn sse_estimated_input_tokens(events: &[Value]) -> AnyResult<u64> {
    let mut estimates = Vec::new();
    for event in events {
        if event.get("type").and_then(Value::as_str) != Some("context_meta") {
            continue;
        }
        if let Some(value) = event
            .pointer("/context_manifest_trace/wire/budget/estimated_input_tokens")
            .and_then(Value::as_u64)
        {
            if value == 0 {
                return Err(invalid("SSE wire budget estimate is zero"));
            }
            estimates.push(value);
        }
    }
    estimates
        .last()
        .copied()
        .ok_or_else(|| invalid("SSE omitted authoritative wire budget estimate"))
}

async fn scrape_admission_counters(app: &Router) -> AnyResult<AdmissionCounters> {
    let request = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())?;
    let response = app.clone().oneshot(request).await?;
    if response.status() != StatusCode::OK {
        return Err(invalid(format!("/metrics returned {}", response.status())));
    }
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| invalid(format!("/metrics is not UTF-8: {error}")))?;
    Ok(AdmissionCounters {
        attempts: exact_prometheus_counter(text, ADMISSION_ATTEMPTS_METRIC)?,
        wait_ms: exact_prometheus_counter(text, ADMISSION_WAIT_MS_METRIC)?,
        units: exact_prometheus_counter(text, ADMISSION_UNITS_METRIC)?,
    })
}

pub(super) fn exact_prometheus_counter(text: &str, metric: &str) -> AnyResult<u64> {
    let type_declaration = format!("# TYPE {metric} counter");
    if !text.lines().any(|line| line == type_declaration) {
        return Err(invalid(format!(
            "missing admission metric family declaration: {type_declaration}"
        )));
    }
    let prefix = format!("{metric}{{outcome=\"acquired\"}} ");
    let mut values = text
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(str::trim);
    let Some(raw) = values.next() else {
        // MetricsRegistry registers the counter family before the first
        // observation, but it does not materialize an `outcome="acquired"`
        // series until that label-set is incremented. A declared family with
        // no exact sample is therefore the authoritative pre-observation zero.
        return Ok(0);
    };
    if values.next().is_some() {
        return Err(invalid(format!(
            "duplicate exact admission metric sample: {prefix}"
        )));
    }
    raw.parse::<u64>()
        .map_err(|error| invalid(format!("invalid counter value for {metric}: {error}")))
}

fn tenant_admission_evidence(
    owner_id: &str,
    before: AdmissionCounters,
    after: AdmissionCounters,
    completed_requests: u64,
) -> AnyResult<TenantAdmissionEvidence> {
    let attempts = after
        .attempts
        .checked_sub(before.attempts)
        .ok_or_else(|| invalid("admission attempt counter regressed"))?;
    let wait_ms = after
        .wait_ms
        .checked_sub(before.wait_ms)
        .ok_or_else(|| invalid("admission wait counter regressed"))?;
    let admission_units = after
        .units
        .checked_sub(before.units)
        .ok_or_else(|| invalid("admission unit counter regressed"))?;
    if attempts != completed_requests || admission_units == 0 {
        return Err(invalid(format!(
            "owner {owner_id} admission interval mismatch: attempts={attempts}, completed={completed_requests}, units={admission_units}"
        )));
    }
    Ok(TenantAdmissionEvidence {
        owner_id: owner_id.to_string(),
        admission_units,
        wait_micros: wait_ms
            .checked_mul(1_000)
            .ok_or_else(|| invalid("admission wait microseconds overflowed"))?,
        completed_requests,
    })
}

fn work_evidence(report: &HistoryWorkScenarioReport) -> AnyResult<ScenarioWorkEvidence> {
    let mut events = 0_u64;
    let mut bytes = 0_u64;
    let mut rows = 0_u64;
    let mut admission_units = 0_u64;
    let mut queue_current = 0_i128;
    let mut queue_peak = 0_u64;
    let mut accounting_errors = 0_u64;
    for site in HistoryWorkSite::ALL {
        let measurement = report.scoped.measurement(site);
        events = checked_add(events, measurement.events, "history-work events")?;
        bytes = checked_add(bytes, measurement.bytes, "history-work bytes")?;
        rows = checked_add(rows, measurement.rows, "history-work rows")?;
        admission_units = checked_add(
            admission_units,
            measurement.admission_units,
            "history-work admission units",
        )?;
        queue_current = queue_current
            .checked_add(i128::from(measurement.queue_current_bytes))
            .ok_or_else(|| invalid("queue current byte sum overflowed"))?;
        queue_peak = queue_peak.max(measurement.queue_peak_bytes);
        accounting_errors = checked_add(
            accounting_errors,
            measurement.accounting_errors,
            "history-work accounting errors",
        )?;
    }
    if events == 0 || bytes == 0 || rows == 0 || admission_units == 0 || queue_peak == 0 {
        return Err(invalid(format!(
            "incomplete history-work report: events={events}, bytes={bytes}, rows={rows}, admission={admission_units}, queue_peak={queue_peak}"
        )));
    }
    if queue_current != 0 || accounting_errors != 0 {
        return Err(invalid(format!(
            "invalid history-work accounting: queue_current={queue_current}, errors={accounting_errors}"
        )));
    }
    Ok(ScenarioWorkEvidence {
        history_events: events,
        clone_hash_serialization_bytes: bytes,
        db_rows: rows,
        admission_units,
        queue_peak_bytes: queue_peak,
        queue_current_bytes_change: queue_current,
        accounting_errors,
    })
}

fn required_output_dir() -> AnyResult<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os(OUTPUT_DIR_ENV)
            .ok_or_else(|| invalid(format!("{OUTPUT_DIR_ENV} must be set")))?,
    );
    if !path.is_dir() {
        return Err(invalid(format!(
            "{OUTPUT_DIR_ENV} must name an existing directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn required_baseline_run_id() -> AnyResult<String> {
    let value = std::env::var(BASELINE_RUN_ID_ENV)
        .map_err(|_| invalid(format!("{BASELINE_RUN_ID_ENV} is required")))?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "{BASELINE_RUN_ID_ENV} must be a 64-character hexadecimal identifier"
        )));
    }
    Ok(value)
}

fn require_exact_env(name: &str, expected: &str) -> AnyResult<()> {
    let actual = std::env::var(name).unwrap_or_default();
    if actual != expected {
        return Err(invalid(format!(
            "{name} must equal {expected:?}; got {actual:?}"
        )));
    }
    Ok(())
}

pub(super) const fn exact_context_window(class: WindowClass) -> u64 {
    match class {
        WindowClass::K128 => 128_000,
        WindowClass::K200 => 200_000,
        WindowClass::M1 => 1_000_000,
    }
}

pub(super) const fn window_label(class: WindowClass) -> &'static str {
    match class {
        WindowClass::K128 => "k128",
        WindowClass::K200 => "k200",
        WindowClass::M1 => "m1",
    }
}

fn required_string(value: &Value, key: &str, context: &str) -> AnyResult<String> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{context} missing string {key}")))?;
    required_nonempty(text.to_string(), key)
}

fn required_u64(value: &Value, key: &str, context: &str) -> AnyResult<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("{context} missing non-negative integer {key}")))
}

fn required_nonempty(value: String, field: &str) -> AnyResult<String> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{field} cannot be empty")));
    }
    Ok(value)
}

fn required_i64(value: Option<i64>, field: &str) -> AnyResult<i64> {
    value.ok_or_else(|| invalid(format!("{field} cannot be NULL")))
}

fn non_negative_i64(value: i64, field: &str) -> AnyResult<u64> {
    u64::try_from(value).map_err(|_| invalid(format!("{field} cannot be negative: {value}")))
}

fn positive_i64(value: i64, field: &str) -> AnyResult<u64> {
    let value = non_negative_i64(value, field)?;
    if value == 0 {
        return Err(invalid(format!("{field} must be positive")));
    }
    Ok(value)
}

fn non_negative_u32(value: i64, field: &str) -> AnyResult<u32> {
    u32::try_from(value).map_err(|_| invalid(format!("{field} is outside u32 range: {value}")))
}

fn checked_add(left: u64, right: u64, field: &str) -> AnyResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| invalid(format!("{field} overflowed")))
}

pub(super) fn invalid(message: impl Into<String>) -> AnyError {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offering_document_is_typed_and_canonicalized() {
        let parsed = parse_offering_config(
            r#"{
                "offerings": [
                    {"window_class":"m1","offering_id":" offer-million "},
                    {"window_class":"k128","offering_id":"offer-128"},
                    {"window_class":"k200","offering_id":"offer-200"}
                ]
            }"#,
        )
        .expect("valid typed Offering document");
        assert_eq!(
            parsed
                .iter()
                .map(|entry| (entry.window_class, entry.offering_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (WindowClass::K128, "offer-128"),
                (WindowClass::K200, "offer-200"),
                (WindowClass::M1, "offer-million"),
            ]
        );
    }

    #[test]
    fn offering_document_rejects_missing_duplicate_and_unknown_structure() {
        for raw in [
            r#"{"offerings":[{"window_class":"k128","offering_id":"a"}]}"#,
            r#"{"offerings":[
                {"window_class":"k128","offering_id":"a"},
                {"window_class":"k128","offering_id":"b"},
                {"window_class":"m1","offering_id":"c"}
            ]}"#,
            r#"{"offerings":[
                {"window_class":"k128","offering_id":"same"},
                {"window_class":"k200","offering_id":"same"},
                {"window_class":"m1","offering_id":"c"}
            ]}"#,
            r#"{"offerings":[
                {"window_class":"k128","offering_id":"a","model_name":"guessed"},
                {"window_class":"k200","offering_id":"b"},
                {"window_class":"m1","offering_id":"c"}
            ]}"#,
        ] {
            assert!(parse_offering_config(raw).is_err(), "accepted: {raw}");
        }
    }

    #[test]
    fn exact_window_classes_do_not_accept_nearby_or_binary_k_values() {
        assert_eq!(exact_context_window(WindowClass::K128), 128_000);
        assert_eq!(exact_context_window(WindowClass::K200), 200_000);
        assert_eq!(exact_context_window(WindowClass::M1), 1_000_000);
        assert_ne!(exact_context_window(WindowClass::K128), 131_072);
    }

    #[test]
    fn parses_only_typed_sse_data_and_rejects_malformed_fixture() {
        let events = parse_sse_events(
            "event: message\n\
             data: {\"type\":\"session_info\",\"session_id\":\"s\",\"run_id\":\"r\"}\n\n\
             data: {\"type\":\"context_meta\",\"context_manifest_trace\":{\"wire\":{\"budget\":{\"estimated_input_tokens\":42}}}}\n\n",
        )
        .expect("typed fixture");
        assert_eq!(events.len(), 2);
        assert_eq!(sse_estimated_input_tokens(&events).unwrap(), 42);
        assert!(parse_sse_events("data: not-json\n\n").is_err());
        assert!(parse_sse_events("data: {\"message\":\"untyped\"}\n\n").is_err());
    }

    #[test]
    fn prometheus_parser_requires_one_exact_acquired_sample() {
        let fixture = "\
# TYPE astra_run_admission_wait_ms_total counter\n\
astra_run_admission_wait_ms_total{outcome=\"acquired\"} 17\n\
astra_run_admission_wait_ms_total{outcome=\"timeout\"} 99\n";
        assert_eq!(
            exact_prometheus_counter(fixture, ADMISSION_WAIT_MS_METRIC).unwrap(),
            17
        );
        assert!(
            exact_prometheus_counter(fixture, "astra_run_admission_weight_units_total").is_err()
        );
    }

    #[test]
    fn prometheus_declared_family_without_acquired_series_is_zero() {
        let fixture = "\
# TYPE astra_run_admission_attempts_total counter\n\
astra_run_admission_attempts_total{outcome=\"timeout\"} 1\n";
        assert_eq!(
            exact_prometheus_counter(fixture, ADMISSION_ATTEMPTS_METRIC).unwrap(),
            0
        );
        assert!(exact_prometheus_counter("", ADMISSION_ATTEMPTS_METRIC).is_err());
    }

    fn admitted_provider_attempt() -> Value {
        json!({
            "authority": "exact_serialized_provider_body_v1",
            "request_id": "attempt-1",
            "request_hash": "wire-hash",
            "round": 0,
            "attempt": 0,
            "protocol": "openai_compatible",
            "provider_response_id": null,
            "terminal_status": null,
            "usage": null,
            "error_kind": null,
            "error_message": null,
            "serialized_bytes": 46,
            "composition_bytes": {
                "system": 11,
                "conversation": 13,
                "tool_schema": 17,
                "provider_envelope": 5,
                "total": 46
            },
            "composition_items": {
                "system": 1,
                "conversation": 2,
                "tool_schema": 3
            }
        })
    }

    fn context_meta_with_attempt(attempt: Value) -> Value {
        json!({
            "type": "context_meta",
            "context_manifest_trace": {
                "provider_request_attempts": [attempt]
            }
        })
    }

    #[test]
    fn sse_attempt_lifecycle_promotes_admitted_snapshot_to_terminal_fact() {
        let admitted = admitted_provider_attempt();
        let mut terminal = admitted.clone();
        terminal["provider_response_id"] = json!("response-1");
        terminal["terminal_status"] = json!("succeeded");
        terminal["usage"] = json!({
            "input_tokens": 10,
            "output_tokens": 2,
            "cache_read_tokens": 8,
            "cache_creation_tokens": 0
        });

        let attempts = typed_sse_attempts(
            &[
                context_meta_with_attempt(admitted),
                context_meta_with_attempt(terminal),
            ],
            "run-1",
        )
        .expect("admitted-to-terminal is the producer's typed lifecycle");

        assert_eq!(
            attempts["attempt-1"].terminal_status,
            Some(AttemptTerminalStatus::Succeeded)
        );
        assert_eq!(
            attempts["attempt-1"]
                .usage
                .as_ref()
                .expect("terminal usage")
                .cache_read_tokens,
            8
        );
    }

    #[test]
    fn sse_attempt_lifecycle_rejects_identity_change_and_terminal_regression() {
        let admitted = admitted_provider_attempt();
        let mut changed_identity = admitted.clone();
        changed_identity["request_hash"] = json!("different-wire-hash");
        assert!(
            typed_sse_attempts(
                &[
                    context_meta_with_attempt(admitted.clone()),
                    context_meta_with_attempt(changed_identity),
                ],
                "run-1",
            )
            .is_err()
        );

        let mut terminal = admitted.clone();
        terminal["terminal_status"] = json!("failed");
        terminal["usage"] = json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0
        });
        terminal["error_kind"] = json!("provider");
        assert!(
            typed_sse_attempts(
                &[
                    context_meta_with_attempt(terminal),
                    context_meta_with_attempt(admitted),
                ],
                "run-1",
            )
            .is_err()
        );
    }

    #[test]
    fn absent_compaction_is_an_exact_zero_measurement() {
        let evidence = compaction_evidence(std::iter::empty()).unwrap();
        assert_eq!(evidence.attempts, 0);
        assert_eq!(evidence.effective_attempts, 0);
        assert_eq!(evidence.input_tokens, 0);
        assert_eq!(evidence.output_tokens, 0);
        assert_eq!(evidence.tokens_freed, 0);
    }

    #[test]
    fn compaction_id_is_deduplicated_within_but_not_across_streams() {
        let first = json!({
            "type": "context_meta",
            "compactions": [{
                "id": "wire-1",
                "kind": "wire_assembly",
                "tier": "compact_history",
                "messages_before": 20,
                "messages_after": 8,
                "tokens_before": 1000,
                "tokens_after": 600,
                "tokens_saved": 400
            }]
        });
        let second = json!({
            "type": "context_meta",
            "compactions": [{
                "id": "wire-1",
                "kind": "wire_assembly",
                "tier": "compact_history",
                "messages_before": 16,
                "messages_after": 6,
                "tokens_before": 800,
                "tokens_after": 500,
                "tokens_saved": 300
            }]
        });
        let changed_same_stream = [first.clone(), second.clone()];
        assert!(
            compaction_evidence([("turn-a", changed_same_stream.as_slice())].into_iter()).is_err()
        );

        let first_stream = [first.clone(), first];
        let second_stream = [second];
        let evidence = compaction_evidence(
            [
                ("turn-a", first_stream.as_slice()),
                ("turn-b", second_stream.as_slice()),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(evidence.attempts, 2);
        assert_eq!(evidence.effective_attempts, 2);
        assert_eq!(evidence.tokens_freed, 700);
    }
}
