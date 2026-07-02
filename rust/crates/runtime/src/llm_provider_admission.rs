use std::{
    env,
    sync::atomic::{AtomicI64, Ordering},
    sync::{Arc, OnceLock, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use astra_core::{ClassifiedError, ErrorKind, SharedPool};
use astra_turn_core::pipeline_metrics::MetricsRegistry;
use serde_json::{Map, Value, json};
use sqlx::Row;

const ENV_MODE: &str = "ASTRA_LLM_PROVIDER_ADMISSION_MODE";
const ENV_RPM: &str = "ASTRA_LLM_PROVIDER_ADMISSION_RPM";
const ENV_CAPACITY_RPM: &str = "ASTRA_CAPACITY_PROVIDER_RPM";
const ENV_TPM: &str = "ASTRA_LLM_PROVIDER_ADMISSION_TPM";
const ENV_CAPACITY_TPM: &str = "ASTRA_CAPACITY_PROVIDER_TPM";
const ENV_WINDOW_MS: &str = "ASTRA_LLM_PROVIDER_ADMISSION_WINDOW_MS";
const ENV_RETENTION_WINDOWS: &str = "ASTRA_LLM_PROVIDER_ADMISSION_RETENTION_WINDOWS";
const ENV_CLEANUP_INTERVAL_MS: &str = "ASTRA_LLM_PROVIDER_ADMISSION_CLEANUP_INTERVAL_MS";
const ENV_SCOPE: &str = "ASTRA_LLM_PROVIDER_ADMISSION_SCOPE";
const ENV_FAIL_OPEN: &str = "ASTRA_LLM_PROVIDER_ADMISSION_FAIL_OPEN";
const ENV_BURST: &str = "ASTRA_LLM_PROVIDER_ADMISSION_BURST";

const DEFAULT_WINDOW_MS: u64 = 60_000;
const DEFAULT_RETENTION_WINDOWS: u64 = 120;
const DEFAULT_CLEANUP_INTERVAL_MS: u64 = 300_000;
const DEFAULT_RPM_BURST: u64 = 1;
const MAX_BUCKET_KEY_BYTES: usize = 240;

const METRIC_PROVIDER_ADMISSION_ATTEMPTS_TOTAL: &str =
    "astra_llm_provider_admission_attempts_total";
const METRIC_PROVIDER_ADMISSION_ERRORS_TOTAL: &str = "astra_llm_provider_admission_errors_total";
const METRIC_PROVIDER_ADMISSION_RETRY_AFTER_MS_TOTAL: &str =
    "astra_llm_provider_admission_retry_after_ms_total";
const METRIC_PROVIDER_ADMISSION_TOKENS_TOTAL: &str = "astra_llm_provider_admission_tokens_total";
const METRIC_PROVIDER_ADMISSION_CLEANUP_RUNS_TOTAL: &str =
    "astra_llm_provider_admission_cleanup_runs_total";
const METRIC_PROVIDER_ADMISSION_CLEANUP_ROWS_TOTAL: &str =
    "astra_llm_provider_admission_cleanup_rows_total";
const METRIC_PROVIDER_ADMISSION_CALIBRATION_SAMPLES_TOTAL: &str =
    "astra_llm_provider_admission_calibration_samples_total";
const METRIC_PROVIDER_ADMISSION_CALIBRATION_ESTIMATED_TOKENS_TOTAL: &str =
    "astra_llm_provider_admission_calibration_estimated_tokens_total";
const METRIC_PROVIDER_ADMISSION_CALIBRATION_ACTUAL_TOKENS_TOTAL: &str =
    "astra_llm_provider_admission_calibration_actual_tokens_total";
const METRIC_PROVIDER_ADMISSION_CALIBRATION_ABS_ERROR_TOKENS_TOTAL: &str =
    "astra_llm_provider_admission_calibration_abs_error_tokens_total";

const CREATE_WINDOWS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS llm_provider_admission_windows (
    bucket_key VARCHAR(255) NOT NULL,
    window_start_ms BIGINT NOT NULL,
    request_count BIGINT NOT NULL DEFAULT 0,
    token_count BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (bucket_key, window_start_ms),
    INDEX idx_llm_provider_admission_windows_updated (updated_at)
)
"#;

const CREATE_PACING_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS llm_provider_admission_pacing (
    bucket_key VARCHAR(255) NOT NULL,
    tat_ms BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (bucket_key)
)
"#;

const INSERT_WINDOW_SQL: &str = r#"
INSERT IGNORE INTO llm_provider_admission_windows
    (bucket_key, window_start_ms, request_count, token_count)
VALUES (?, ?, 0, 0)
"#;

const INSERT_PACING_SQL: &str = r#"
INSERT IGNORE INTO llm_provider_admission_pacing
    (bucket_key, tat_ms)
VALUES (?, 0)
"#;

const CLAIM_PACING_SQL: &str = r#"
UPDATE llm_provider_admission_pacing
SET tat_ms = (CASE WHEN tat_ms > ? THEN tat_ms ELSE ? END) + ?,
    updated_at = CURRENT_TIMESTAMP(6)
WHERE bucket_key = ?
  AND tat_ms <= ?
"#;

const SELECT_PACING_SQL: &str = r#"
SELECT tat_ms
FROM llm_provider_admission_pacing
WHERE bucket_key = ?
"#;

const CLAIM_WINDOW_SLOT_RPM_SQL: &str = r#"
UPDATE llm_provider_admission_windows
SET request_count = request_count + 1,
    token_count = token_count + ?,
    updated_at = CURRENT_TIMESTAMP(6)
WHERE bucket_key = ?
  AND window_start_ms = ?
  AND request_count < ?
"#;

const CLAIM_WINDOW_SLOT_TPM_SQL: &str = r#"
UPDATE llm_provider_admission_windows
SET request_count = request_count + 1,
    token_count = token_count + ?,
    updated_at = CURRENT_TIMESTAMP(6)
WHERE bucket_key = ?
  AND window_start_ms = ?
  AND token_count + ? <= ?
"#;

const CLAIM_WINDOW_SLOT_RPM_TPM_SQL: &str = r#"
UPDATE llm_provider_admission_windows
SET request_count = request_count + 1,
    token_count = token_count + ?,
    updated_at = CURRENT_TIMESTAMP(6)
WHERE bucket_key = ?
  AND window_start_ms = ?
  AND request_count < ?
  AND token_count + ? <= ?
"#;

const SELECT_WINDOW_COUNTS_SQL: &str = r#"
SELECT request_count, token_count
FROM llm_provider_admission_windows
WHERE bucket_key = ?
  AND window_start_ms = ?
"#;

const CLEANUP_WINDOWS_SQL: &str = r#"
DELETE FROM llm_provider_admission_windows
WHERE window_start_ms < ?
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderAdmissionConfig {
    mode: ProviderAdmissionMode,
    rpm_limit: Option<u64>,
    tpm_limit: Option<u64>,
    window_ms: u64,
    retention_windows: u64,
    cleanup_interval_ms: u64,
    scope: ProviderAdmissionScope,
    fail_open: bool,
    burst: u64,
}

impl ProviderAdmissionConfig {
    pub(crate) fn from_env() -> Self {
        let mode = env::var(ENV_MODE)
            .ok()
            .as_deref()
            .map(parse_mode)
            .unwrap_or(ProviderAdmissionMode::Disabled);
        Self {
            mode,
            rpm_limit: read_positive_u64(ENV_RPM).or_else(|| read_positive_u64(ENV_CAPACITY_RPM)),
            tpm_limit: read_positive_u64(ENV_TPM).or_else(|| read_positive_u64(ENV_CAPACITY_TPM)),
            window_ms: read_positive_u64(ENV_WINDOW_MS).unwrap_or(DEFAULT_WINDOW_MS),
            retention_windows: read_positive_u64(ENV_RETENTION_WINDOWS)
                .unwrap_or(DEFAULT_RETENTION_WINDOWS),
            cleanup_interval_ms: read_positive_u64(ENV_CLEANUP_INTERVAL_MS)
                .unwrap_or(DEFAULT_CLEANUP_INTERVAL_MS),
            scope: env::var(ENV_SCOPE)
                .ok()
                .as_deref()
                .map(parse_scope)
                .unwrap_or(ProviderAdmissionScope::Provider),
            fail_open: read_bool(ENV_FAIL_OPEN).unwrap_or(false),
            burst: read_positive_u64(ENV_BURST).unwrap_or(DEFAULT_RPM_BURST),
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            mode: ProviderAdmissionMode::Disabled,
            rpm_limit: None,
            tpm_limit: None,
            window_ms: DEFAULT_WINDOW_MS,
            retention_windows: DEFAULT_RETENTION_WINDOWS,
            cleanup_interval_ms: DEFAULT_CLEANUP_INTERVAL_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
            burst: DEFAULT_RPM_BURST,
        }
    }

    #[cfg(test)]
    fn db_fixed_window(rpm_limit: Option<u64>) -> Self {
        Self {
            mode: ProviderAdmissionMode::DbFixedWindow,
            rpm_limit,
            tpm_limit: None,
            window_ms: DEFAULT_WINDOW_MS,
            retention_windows: DEFAULT_RETENTION_WINDOWS,
            cleanup_interval_ms: DEFAULT_CLEANUP_INTERVAL_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
            burst: DEFAULT_RPM_BURST,
        }
    }

    #[cfg(test)]
    fn db_fixed_window_with_tpm(rpm_limit: Option<u64>, tpm_limit: Option<u64>) -> Self {
        Self {
            mode: ProviderAdmissionMode::DbFixedWindow,
            rpm_limit,
            tpm_limit,
            window_ms: DEFAULT_WINDOW_MS,
            retention_windows: DEFAULT_RETENTION_WINDOWS,
            cleanup_interval_ms: DEFAULT_CLEANUP_INTERVAL_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
            burst: DEFAULT_RPM_BURST,
        }
    }

    #[cfg(test)]
    fn unsupported() -> Self {
        Self {
            mode: ProviderAdmissionMode::Unsupported,
            rpm_limit: None,
            tpm_limit: None,
            window_ms: DEFAULT_WINDOW_MS,
            retention_windows: DEFAULT_RETENTION_WINDOWS,
            cleanup_interval_ms: DEFAULT_CLEANUP_INTERVAL_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
            burst: DEFAULT_RPM_BURST,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !matches!(self.mode, ProviderAdmissionMode::Disabled)
    }

    pub(crate) fn fail_open(&self) -> bool {
        self.fail_open
    }

    fn mode_label(&self) -> &'static str {
        self.mode.as_label()
    }

    fn scope_label(&self) -> &'static str {
        self.scope.as_label()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderAdmissionMode {
    Disabled,
    DbFixedWindow,
    Unsupported,
}

impl ProviderAdmissionMode {
    fn as_label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::DbFixedWindow => "db_fixed_window",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderAdmissionScope {
    Provider,
    ProviderModel,
}

impl ProviderAdmissionScope {
    fn as_label(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::ProviderModel => "provider_model",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixedWindowAdmission {
    Admitted,
    Rejected {
        retry_after_ms: u64,
        limit: ProviderAdmissionLimit,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WindowCounts {
    request_count: u64,
    token_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderAdmissionLimit {
    Rpm,
    Tpm,
    Capacity,
}

impl ProviderAdmissionLimit {
    fn as_label(self) -> &'static str {
        match self {
            Self::Rpm => "rpm",
            Self::Tpm => "tpm",
            Self::Capacity => "capacity",
        }
    }

    fn rejected_outcome(self) -> &'static str {
        match self {
            Self::Rpm => "rejected_rpm",
            Self::Tpm => "rejected_tpm",
            Self::Capacity => "rejected_capacity",
        }
    }
}

fn metrics_slot() -> &'static RwLock<Option<Arc<MetricsRegistry>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<MetricsRegistry>>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

pub(crate) fn set_llm_provider_admission_metrics_registry(registry: Arc<MetricsRegistry>) {
    register_provider_admission_metrics(&registry);
    *metrics_slot()
        .write()
        .expect("llm provider admission metrics registry lock poisoned") = Some(registry);
}

fn provider_admission_metrics_registry() -> Option<Arc<MetricsRegistry>> {
    metrics_slot()
        .read()
        .expect("llm provider admission metrics registry lock poisoned")
        .clone()
}

fn register_provider_admission_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_ATTEMPTS_TOTAL,
        "LLM provider admission attempts by mode, scope, and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_ERRORS_TOTAL,
        "LLM provider admission errors by mode, scope, class, and fail-open policy.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_RETRY_AFTER_MS_TOTAL,
        "Total retry-after milliseconds returned by rejected LLM provider admission attempts.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_TOKENS_TOTAL,
        "Estimated LLM provider admission tokens by mode, scope, and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_CLEANUP_RUNS_TOTAL,
        "LLM provider admission window cleanup runs by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_CLEANUP_ROWS_TOTAL,
        "LLM provider admission window cleanup deleted rows by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_SAMPLES_TOTAL,
        "LLM provider admission estimate calibration samples by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_ESTIMATED_TOKENS_TOTAL,
        "Total estimated tokens for LLM provider admission calibration by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_ACTUAL_TOKENS_TOTAL,
        "Total provider-reported actual tokens for LLM provider admission calibration by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_ABS_ERROR_TOKENS_TOTAL,
        "Total absolute token-estimate error for LLM provider admission calibration by low-cardinality outcome.",
    );
}

fn record_attempt(config: &ProviderAdmissionConfig, outcome: &'static str) {
    let Some(registry) = provider_admission_metrics_registry() else {
        return;
    };
    register_provider_admission_metrics(&registry);
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_ATTEMPTS_TOTAL,
        &[
            ("mode", config.mode_label()),
            ("scope", config.scope_label()),
            ("outcome", outcome),
        ],
        1,
    );
}

fn record_error(config: &ProviderAdmissionConfig, class: &'static str) {
    let Some(registry) = provider_admission_metrics_registry() else {
        return;
    };
    register_provider_admission_metrics(&registry);
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_ERRORS_TOTAL,
        &[
            ("mode", config.mode_label()),
            ("scope", config.scope_label()),
            ("class", class),
            (
                "policy",
                if config.fail_open {
                    "fail_open"
                } else {
                    "fail_closed"
                },
            ),
        ],
        1,
    );
}

fn record_retry_after(config: &ProviderAdmissionConfig, retry_after_ms: u64) {
    let Some(registry) = provider_admission_metrics_registry() else {
        return;
    };
    register_provider_admission_metrics(&registry);
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_RETRY_AFTER_MS_TOTAL,
        &[
            ("mode", config.mode_label()),
            ("scope", config.scope_label()),
        ],
        retry_after_ms,
    );
}

fn record_tokens(config: &ProviderAdmissionConfig, outcome: &'static str, estimated_tokens: u64) {
    let Some(registry) = provider_admission_metrics_registry() else {
        return;
    };
    register_provider_admission_metrics(&registry);
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_TOKENS_TOTAL,
        &[
            ("mode", config.mode_label()),
            ("scope", config.scope_label()),
            ("outcome", outcome),
        ],
        estimated_tokens,
    );
}

fn record_cleanup(outcome: &'static str, rows: u64) {
    let Some(registry) = provider_admission_metrics_registry() else {
        return;
    };
    register_provider_admission_metrics(&registry);
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_CLEANUP_RUNS_TOTAL,
        &[("outcome", outcome)],
        1,
    );
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_CLEANUP_ROWS_TOTAL,
        &[("outcome", outcome)],
        rows,
    );
}

pub(crate) fn record_llm_provider_admission_calibration(
    estimated_tokens: u64,
    usage: &Map<String, Value>,
) {
    let Some(registry) = provider_admission_metrics_registry() else {
        return;
    };
    register_provider_admission_metrics(&registry);
    let estimated_tokens = estimated_tokens.max(1);
    let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(usage);
    let actual_tokens = usage.total_tokens();
    let outcome = calibration_outcome(estimated_tokens, actual_tokens);
    let absolute_error = estimated_tokens.abs_diff(actual_tokens);
    let labels = &[("outcome", outcome)];
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_SAMPLES_TOTAL,
        labels,
        1,
    );
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_ESTIMATED_TOKENS_TOTAL,
        labels,
        estimated_tokens,
    );
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_ACTUAL_TOKENS_TOTAL,
        labels,
        actual_tokens,
    );
    registry.increment_counter(
        METRIC_PROVIDER_ADMISSION_CALIBRATION_ABS_ERROR_TOKENS_TOTAL,
        labels,
        absolute_error,
    );
}

fn calibration_outcome(estimated_tokens: u64, actual_tokens: u64) -> &'static str {
    if actual_tokens == 0 {
        return "missing_usage";
    }
    let absolute_error = estimated_tokens.abs_diff(actual_tokens);
    if absolute_error.saturating_mul(10) <= actual_tokens {
        "within_10pct"
    } else if estimated_tokens > actual_tokens {
        "overestimate"
    } else {
        "underestimate"
    }
}

pub(crate) async fn ensure_llm_provider_admission_schema_if_configured(
    shared_pool: &SharedPool,
) -> Result<(), ClassifiedError> {
    let config = ProviderAdmissionConfig::from_env();
    validate_startup_config(&config)?;
    if !matches!(config.mode, ProviderAdmissionMode::DbFixedWindow) {
        return Ok(());
    }
    sqlx::query(CREATE_WINDOWS_TABLE_SQL)
        .execute(shared_pool.get())
        .await
        .map_err(|error| {
            ClassifiedError::new(
                ErrorKind::DatabaseError,
                format!("LLM provider admission schema init failed: {error}"),
            )
        })?;
    sqlx::query(CREATE_PACING_TABLE_SQL)
        .execute(shared_pool.get())
        .await
        .map_err(|error| {
            ClassifiedError::new(
                ErrorKind::DatabaseError,
                format!("LLM provider admission pacing schema init failed: {error}"),
            )
        })?;
    Ok(())
}

fn validate_startup_config(config: &ProviderAdmissionConfig) -> Result<(), ClassifiedError> {
    match config.mode {
        ProviderAdmissionMode::Disabled => Ok(()),
        ProviderAdmissionMode::Unsupported => Err(ClassifiedError::new(
            ErrorKind::InvalidRequest,
            format!(
                "Unsupported {ENV_MODE}; use disabled or db_fixed_window for LLM provider admission."
            ),
        )),
        ProviderAdmissionMode::DbFixedWindow
            if config.rpm_limit.is_none() && config.tpm_limit.is_none() =>
        {
            Err(ClassifiedError::new(
                ErrorKind::InvalidRequest,
                format!(
                    "{ENV_MODE}=db_fixed_window requires {ENV_RPM}/{ENV_CAPACITY_RPM} or {ENV_TPM}/{ENV_CAPACITY_TPM}."
                ),
            ))
        }
        ProviderAdmissionMode::DbFixedWindow => Ok(()),
    }
}

pub(crate) async fn admit_llm_provider_request(
    shared_pool: Option<&SharedPool>,
    provider: &str,
    model: &str,
    estimated_tokens: u64,
) -> Result<(), ClassifiedError> {
    let config = ProviderAdmissionConfig::from_env();
    admit_llm_provider_request_with_config(shared_pool, provider, model, estimated_tokens, config)
        .await
}

async fn admit_llm_provider_request_with_config(
    shared_pool: Option<&SharedPool>,
    provider: &str,
    model: &str,
    estimated_tokens: u64,
    config: ProviderAdmissionConfig,
) -> Result<(), ClassifiedError> {
    let estimated_tokens = estimated_tokens.max(1);
    match config.mode {
        ProviderAdmissionMode::Disabled => {
            record_attempt(&config, "disabled");
            record_tokens(&config, "disabled", estimated_tokens);
            Ok(())
        }
        ProviderAdmissionMode::Unsupported => handle_admission_error(
            &config,
            "unsupported_mode",
            ClassifiedError::new(
                ErrorKind::InvalidRequest,
                format!(
                    "Unsupported {ENV_MODE}; use disabled or db_fixed_window for LLM provider admission."
                ),
            ),
            Some(estimated_tokens),
        ),
        ProviderAdmissionMode::DbFixedWindow => {
            if config.rpm_limit.is_none() && config.tpm_limit.is_none() {
                return handle_admission_error(
                    &config,
                    "misconfigured",
                    ClassifiedError::new(
                        ErrorKind::InvalidRequest,
                        format!(
                            "{ENV_MODE}=db_fixed_window requires {ENV_RPM}/{ENV_CAPACITY_RPM} or {ENV_TPM}/{ENV_CAPACITY_TPM}."
                        ),
                    ),
                    Some(estimated_tokens),
                );
            }
            let Some(shared_pool) = shared_pool else {
                return handle_admission_error(
                    &config,
                    "missing_pool",
                    ClassifiedError::new(
                        ErrorKind::DatabaseError,
                        "LLM provider admission is enabled but no shared database pool is available.",
                    ),
                    Some(estimated_tokens),
                );
            };
            match db_fixed_window_admit(shared_pool, provider, model, &config, estimated_tokens)
                .await
            {
                Ok(FixedWindowAdmission::Admitted) => {
                    record_attempt(&config, "admitted");
                    record_tokens(&config, "admitted", estimated_tokens);
                    Ok(())
                }
                Ok(FixedWindowAdmission::Rejected {
                    retry_after_ms,
                    limit,
                }) => {
                    record_attempt(&config, limit.rejected_outcome());
                    record_tokens(&config, limit.rejected_outcome(), estimated_tokens);
                    record_retry_after(&config, retry_after_ms);
                    Err(provider_admission_rejection_error(
                        &config,
                        limit,
                        retry_after_ms,
                        estimated_tokens,
                    ))
                }
                Err(error) => {
                    handle_admission_error(&config, "database_error", error, Some(estimated_tokens))
                }
            }
        }
    }
}

fn handle_admission_error(
    config: &ProviderAdmissionConfig,
    class: &'static str,
    error: ClassifiedError,
    estimated_tokens: Option<u64>,
) -> Result<(), ClassifiedError> {
    record_error(config, class);
    if config.fail_open {
        record_attempt(config, "error_fail_open");
        if let Some(estimated_tokens) = estimated_tokens {
            record_tokens(config, "error_fail_open", estimated_tokens);
        }
        Ok(())
    } else {
        record_attempt(config, "error_fail_closed");
        if let Some(estimated_tokens) = estimated_tokens {
            record_tokens(config, "error_fail_closed", estimated_tokens);
        }
        Err(error)
    }
}

fn provider_admission_rejection_error(
    config: &ProviderAdmissionConfig,
    limit: ProviderAdmissionLimit,
    retry_after_ms: u64,
    estimated_tokens: u64,
) -> ClassifiedError {
    let rpm = config
        .rpm_limit
        .map_or("none".to_string(), |value| value.to_string());
    let tpm = config
        .tpm_limit
        .map_or("none".to_string(), |value| value.to_string());
    let details = json!({
        "source": "llm_provider_admission",
        "policy": "fail_fast",
        "limit": limit.as_label(),
        "scope": config.scope_label(),
        "mode": config.mode_label(),
        "retry_after_ms": retry_after_ms,
        "estimated_tokens": estimated_tokens.max(1),
        "rpm_limit": config.rpm_limit,
        "tpm_limit": config.tpm_limit,
        "window_ms": config.window_ms,
    });
    ClassifiedError::new(
        ErrorKind::RateLimit,
        format!(
            "LLM provider admission {} limit reached for {} scope (rpm: {}, tpm: {}, window: {}ms). Retry after {}s.",
            limit.as_label(),
            config.scope_label(),
            rpm,
            tpm,
            config.window_ms,
            retry_after_ms.div_ceil(1000)
        ),
    )
    .with_details_json(details.to_string())
}

fn cleanup_timestamp_slot() -> &'static AtomicI64 {
    static SLOT: AtomicI64 = AtomicI64::new(0);
    &SLOT
}

async fn cleanup_stale_windows_if_due(
    shared_pool: &SharedPool,
    config: &ProviderAdmissionConfig,
    now_ms: i64,
) {
    if !should_run_cleanup(now_ms, config.cleanup_interval_ms) {
        return;
    }
    match cleanup_stale_windows(shared_pool, config, now_ms).await {
        Ok(rows) => record_cleanup("ok", rows),
        Err(error) => {
            tracing::warn!(
                target: "astra_runtime::llm_provider_admission",
                error = %error.message,
                "llm provider admission window cleanup failed"
            );
            record_cleanup("error", 0);
        }
    }
}

async fn cleanup_stale_windows(
    shared_pool: &SharedPool,
    config: &ProviderAdmissionConfig,
    now_ms: i64,
) -> Result<u64, ClassifiedError> {
    let cutoff_ms = cleanup_cutoff_ms(now_ms, config.window_ms, config.retention_windows);
    let result = sqlx::query(CLEANUP_WINDOWS_SQL)
        .bind(cutoff_ms)
        .execute(shared_pool.get())
        .await
        .map_err(database_error)?;
    Ok(result.rows_affected())
}

fn should_run_cleanup(now_ms: i64, interval_ms: u64) -> bool {
    let interval_ms = i64::try_from(interval_ms.max(1)).unwrap_or(i64::MAX);
    let slot = cleanup_timestamp_slot();
    let mut last = slot.load(Ordering::Relaxed);
    loop {
        if !cleanup_due(last, now_ms, interval_ms) {
            return false;
        }
        match slot.compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => last = actual,
        }
    }
}

fn cleanup_due(last_cleanup_ms: i64, now_ms: i64, interval_ms: i64) -> bool {
    now_ms.saturating_sub(last_cleanup_ms) >= interval_ms
}

fn cleanup_cutoff_ms(now_ms: i64, window_ms: u64, retention_windows: u64) -> i64 {
    let window_start_ms = fixed_window_start_ms(now_ms, window_ms);
    let window_ms = i64::try_from(window_ms.max(1)).unwrap_or(i64::MAX);
    let retention_windows = i64::try_from(retention_windows.max(1)).unwrap_or(i64::MAX);
    window_start_ms.saturating_sub(window_ms.saturating_mul(retention_windows))
}

async fn db_fixed_window_admit(
    shared_pool: &SharedPool,
    provider: &str,
    model: &str,
    config: &ProviderAdmissionConfig,
    estimated_tokens: u64,
) -> Result<FixedWindowAdmission, ClassifiedError> {
    let now_ms = now_epoch_ms();
    cleanup_stale_windows_if_due(shared_pool, config, now_ms).await;
    let window_start_ms = fixed_window_start_ms(now_ms, config.window_ms);
    let bucket_key = bucket_key(config.scope, provider, model);
    let estimated_tokens_i64 = i64::try_from(estimated_tokens).unwrap_or(i64::MAX);
    if let Some(retry_after_ms) = claim_rpm_pacing(shared_pool, &bucket_key, config, now_ms).await?
    {
        return Ok(FixedWindowAdmission::Rejected {
            retry_after_ms,
            limit: ProviderAdmissionLimit::Rpm,
        });
    }
    let previous_window_start_ms = previous_window_start_ms(window_start_ms, config.window_ms);
    let previous_counts =
        fetch_window_counts(shared_pool, &bucket_key, previous_window_start_ms).await?;
    let previous_weight =
        weighted_previous_window_counts(previous_counts, now_ms, window_start_ms, config.window_ms);
    let rpm_remaining = config
        .rpm_limit
        .map(|limit| remaining_limit_after_previous_weight(limit, previous_weight.request_count));
    let tpm_remaining = config
        .tpm_limit
        .map(|limit| remaining_limit_after_previous_weight(limit, previous_weight.token_count));

    sqlx::query(INSERT_WINDOW_SQL)
        .bind(&bucket_key)
        .bind(window_start_ms)
        .execute(shared_pool.get())
        .await
        .map_err(database_error)?;

    let result = match (config.rpm_limit, config.tpm_limit) {
        (Some(rpm_limit), Some(tpm_limit)) => {
            sqlx::query(CLAIM_WINDOW_SLOT_RPM_TPM_SQL)
                .bind(estimated_tokens_i64)
                .bind(&bucket_key)
                .bind(window_start_ms)
                .bind(rpm_remaining.unwrap_or_else(|| i64::try_from(rpm_limit).unwrap_or(i64::MAX)))
                .bind(estimated_tokens_i64)
                .bind(tpm_remaining.unwrap_or_else(|| i64::try_from(tpm_limit).unwrap_or(i64::MAX)))
                .execute(shared_pool.get())
                .await
        }
        (Some(rpm_limit), None) => {
            sqlx::query(CLAIM_WINDOW_SLOT_RPM_SQL)
                .bind(estimated_tokens_i64)
                .bind(&bucket_key)
                .bind(window_start_ms)
                .bind(rpm_remaining.unwrap_or_else(|| i64::try_from(rpm_limit).unwrap_or(i64::MAX)))
                .execute(shared_pool.get())
                .await
        }
        (None, Some(tpm_limit)) => {
            sqlx::query(CLAIM_WINDOW_SLOT_TPM_SQL)
                .bind(estimated_tokens_i64)
                .bind(&bucket_key)
                .bind(window_start_ms)
                .bind(estimated_tokens_i64)
                .bind(tpm_remaining.unwrap_or_else(|| i64::try_from(tpm_limit).unwrap_or(i64::MAX)))
                .execute(shared_pool.get())
                .await
        }
        (None, None) => {
            return Err(ClassifiedError::new(
                ErrorKind::InvalidRequest,
                "LLM provider admission DB fixed-window mode has no RPM or TPM limit.",
            ));
        }
    }
    .map_err(database_error)?;

    if result.rows_affected() == 1 {
        Ok(FixedWindowAdmission::Admitted)
    } else {
        let limit = detect_rejected_limit(
            shared_pool,
            &bucket_key,
            window_start_ms,
            previous_weight,
            config,
            estimated_tokens,
        )
        .await?;
        Ok(FixedWindowAdmission::Rejected {
            retry_after_ms: retry_after_ms(now_ms, window_start_ms, config.window_ms),
            limit,
        })
    }
}

async fn detect_rejected_limit(
    shared_pool: &SharedPool,
    bucket_key: &str,
    window_start_ms: i64,
    previous_weight: WindowCounts,
    config: &ProviderAdmissionConfig,
    estimated_tokens: u64,
) -> Result<ProviderAdmissionLimit, ClassifiedError> {
    let current_counts = fetch_window_counts(shared_pool, bucket_key, window_start_ms).await?;
    Ok(rejected_limit_from_counts(
        current_counts
            .request_count
            .saturating_add(previous_weight.request_count),
        current_counts
            .token_count
            .saturating_add(previous_weight.token_count),
        estimated_tokens,
        config.rpm_limit,
        config.tpm_limit,
    ))
}

async fn fetch_window_counts(
    shared_pool: &SharedPool,
    bucket_key: &str,
    window_start_ms: i64,
) -> Result<WindowCounts, ClassifiedError> {
    let row = sqlx::query(SELECT_WINDOW_COUNTS_SQL)
        .bind(bucket_key)
        .bind(window_start_ms)
        .fetch_optional(shared_pool.get())
        .await
        .map_err(database_error)?;
    let Some(row) = row else {
        return Ok(WindowCounts::default());
    };
    Ok(WindowCounts {
        request_count: row.try_get::<i64, _>("request_count").unwrap_or(0).max(0) as u64,
        token_count: row.try_get::<i64, _>("token_count").unwrap_or(0).max(0) as u64,
    })
}

async fn claim_rpm_pacing(
    shared_pool: &SharedPool,
    bucket_key: &str,
    config: &ProviderAdmissionConfig,
    now_ms: i64,
) -> Result<Option<u64>, ClassifiedError> {
    let Some(rpm_limit) = config.rpm_limit else {
        return Ok(None);
    };
    let interval_ms = pacing_interval_ms(config.window_ms, rpm_limit);
    let burst_tolerance_ms = pacing_burst_tolerance_ms(interval_ms, config.burst);
    sqlx::query(INSERT_PACING_SQL)
        .bind(bucket_key)
        .execute(shared_pool.get())
        .await
        .map_err(database_error)?;
    let allowed_tat_ms =
        now_ms.saturating_add(i64::try_from(burst_tolerance_ms).unwrap_or(i64::MAX));
    let result = sqlx::query(CLAIM_PACING_SQL)
        .bind(now_ms)
        .bind(now_ms)
        .bind(i64::try_from(interval_ms).unwrap_or(i64::MAX))
        .bind(bucket_key)
        .bind(allowed_tat_ms)
        .execute(shared_pool.get())
        .await
        .map_err(database_error)?;
    if result.rows_affected() == 1 {
        return Ok(None);
    }
    let retry_after_ms =
        fetch_pacing_retry_after_ms(shared_pool, bucket_key, now_ms, burst_tolerance_ms).await?;
    Ok(Some(retry_after_ms))
}

async fn fetch_pacing_retry_after_ms(
    shared_pool: &SharedPool,
    bucket_key: &str,
    now_ms: i64,
    burst_tolerance_ms: u64,
) -> Result<u64, ClassifiedError> {
    let row = sqlx::query(SELECT_PACING_SQL)
        .bind(bucket_key)
        .fetch_optional(shared_pool.get())
        .await
        .map_err(database_error)?;
    let Some(row) = row else {
        return Ok(1);
    };
    let tat_ms = row.try_get::<i64, _>("tat_ms").unwrap_or(now_ms);
    let eligible_at_ms =
        tat_ms.saturating_sub(i64::try_from(burst_tolerance_ms).unwrap_or(i64::MAX));
    Ok(u64::try_from(eligible_at_ms.saturating_sub(now_ms))
        .unwrap_or(1)
        .max(1))
}

fn rejected_limit_from_counts(
    request_count: u64,
    token_count: u64,
    estimated_tokens: u64,
    rpm_limit: Option<u64>,
    tpm_limit: Option<u64>,
) -> ProviderAdmissionLimit {
    let rpm_exhausted = rpm_limit.is_some_and(|limit| request_count >= limit);
    let tpm_exhausted =
        tpm_limit.is_some_and(|limit| token_count.saturating_add(estimated_tokens.max(1)) > limit);
    match (rpm_exhausted, tpm_exhausted) {
        (true, false) => ProviderAdmissionLimit::Rpm,
        (false, true) => ProviderAdmissionLimit::Tpm,
        _ => ProviderAdmissionLimit::Capacity,
    }
}

fn database_error(error: sqlx::Error) -> ClassifiedError {
    ClassifiedError::new(
        ErrorKind::DatabaseError,
        format!("LLM provider admission database error: {error}"),
    )
}

fn now_epoch_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(i64::MAX as u128) as i64
}

fn fixed_window_start_ms(now_ms: i64, window_ms: u64) -> i64 {
    let window_ms = i64::try_from(window_ms.max(1)).unwrap_or(i64::MAX);
    now_ms - now_ms.rem_euclid(window_ms)
}

fn previous_window_start_ms(window_start_ms: i64, window_ms: u64) -> i64 {
    let window_ms = i64::try_from(window_ms.max(1)).unwrap_or(i64::MAX);
    window_start_ms.saturating_sub(window_ms)
}

fn weighted_previous_window_counts(
    previous: WindowCounts,
    now_ms: i64,
    window_start_ms: i64,
    window_ms: u64,
) -> WindowCounts {
    let window_ms = window_ms.max(1);
    let elapsed_ms = u64::try_from(now_ms.saturating_sub(window_start_ms))
        .unwrap_or(0)
        .min(window_ms);
    WindowCounts {
        request_count: weighted_previous_value(previous.request_count, elapsed_ms, window_ms),
        token_count: weighted_previous_value(previous.token_count, elapsed_ms, window_ms),
    }
}

fn weighted_previous_value(value: u64, elapsed_ms: u64, window_ms: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    let window_ms = window_ms.max(1);
    let remaining_ms = window_ms.saturating_sub(elapsed_ms.min(window_ms));
    if remaining_ms == 0 {
        return 0;
    }
    let numerator = u128::from(value).saturating_mul(u128::from(remaining_ms));
    numerator
        .div_ceil(u128::from(window_ms))
        .min(u128::from(u64::MAX)) as u64
}

fn remaining_limit_after_previous_weight(limit: u64, previous_weight: u64) -> i64 {
    i64::try_from(limit.saturating_sub(previous_weight)).unwrap_or(i64::MAX)
}

fn pacing_interval_ms(window_ms: u64, rpm_limit: u64) -> u64 {
    window_ms.max(1).div_ceil(rpm_limit.max(1)).max(1)
}

fn pacing_burst_tolerance_ms(interval_ms: u64, burst: u64) -> u64 {
    interval_ms.saturating_mul(burst.saturating_sub(1))
}

fn retry_after_ms(now_ms: i64, window_start_ms: i64, window_ms: u64) -> u64 {
    let window_ms = i64::try_from(window_ms.max(1)).unwrap_or(i64::MAX);
    let next_window = window_start_ms.saturating_add(window_ms);
    u64::try_from(next_window.saturating_sub(now_ms))
        .unwrap_or(1)
        .max(1)
}

fn bucket_key(scope: ProviderAdmissionScope, provider: &str, model: &str) -> String {
    let raw = match scope {
        ProviderAdmissionScope::Provider => format!("provider:{}", normalize_key_part(provider)),
        ProviderAdmissionScope::ProviderModel => format!(
            "provider_model:{}:{}",
            normalize_key_part(provider),
            normalize_key_part(model)
        ),
    };
    truncate_to_bytes(raw, MAX_BUCKET_KEY_BYTES)
}

fn normalize_key_part(raw: &str) -> String {
    let normalized: String = raw
        .trim()
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        normalized
    }
}

fn truncate_to_bytes(mut value: String, max_bytes: usize) -> String {
    while value.len() > max_bytes {
        value.pop();
    }
    value
}

fn parse_mode(raw: &str) -> ProviderAdmissionMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "off" | "false" | "0" | "disabled" => ProviderAdmissionMode::Disabled,
        "db" | "database" | "db_fixed_window" | "fixed_window" => {
            ProviderAdmissionMode::DbFixedWindow
        }
        _ => ProviderAdmissionMode::Unsupported,
    }
}

fn parse_scope(raw: &str) -> ProviderAdmissionScope {
    match raw.trim().to_ascii_lowercase().as_str() {
        "provider_model" | "model" | "provider:model" => ProviderAdmissionScope::ProviderModel,
        _ => ProviderAdmissionScope::Provider,
    }
}

fn read_positive_u64(name: &str) -> Option<u64> {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn read_bool(name: &str) -> Option<bool> {
    env::var(name)
        .ok()
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, sync::OnceLock};

    use tokio::sync::Mutex;

    use super::*;

    fn metrics_test_lock() -> Arc<Mutex<()>> {
        static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
        LOCK.get_or_init(|| Arc::new(Mutex::new(()))).clone()
    }

    #[test]
    fn fixed_window_math_is_stable_at_boundaries() {
        assert_eq!(fixed_window_start_ms(123_456, 60_000), 120_000);
        assert_eq!(fixed_window_start_ms(120_000, 60_000), 120_000);
        assert_eq!(previous_window_start_ms(120_000, 60_000), 60_000);
        assert_eq!(retry_after_ms(123_456, 120_000, 60_000), 56_544);
        assert_eq!(retry_after_ms(180_000, 120_000, 60_000), 1);
    }

    #[test]
    fn previous_window_weight_smooths_fixed_window_boundary() {
        let previous = WindowCounts {
            request_count: 20,
            token_count: 60_000,
        };
        let window_start_ms = 180_000;

        let at_boundary =
            weighted_previous_window_counts(previous, 180_000, window_start_ms, 60_000);
        assert_eq!(at_boundary.request_count, 20);
        assert_eq!(at_boundary.token_count, 60_000);
        assert_eq!(
            remaining_limit_after_previous_weight(20, at_boundary.request_count),
            0
        );

        let after_three_seconds =
            weighted_previous_window_counts(previous, 183_000, window_start_ms, 60_000);
        assert_eq!(after_three_seconds.request_count, 19);
        assert_eq!(
            remaining_limit_after_previous_weight(20, after_three_seconds.request_count),
            1
        );

        let halfway = weighted_previous_window_counts(previous, 210_000, window_start_ms, 60_000);
        assert_eq!(halfway.request_count, 10);
        assert_eq!(halfway.token_count, 30_000);

        let at_next_boundary =
            weighted_previous_window_counts(previous, 240_000, window_start_ms, 60_000);
        assert_eq!(at_next_boundary, WindowCounts::default());
    }

    #[test]
    fn rpm_pacing_defaults_to_small_burst_and_stable_interval() {
        assert_eq!(ProviderAdmissionConfig::db_fixed_window(Some(20)).burst, 1);
        assert_eq!(pacing_interval_ms(60_000, 20), 3_000);
        assert_eq!(pacing_interval_ms(60_000, 7), 8_572);
        assert_eq!(pacing_burst_tolerance_ms(3_000, 4), 9_000);
        assert_eq!(pacing_burst_tolerance_ms(3_000, 1), 0);
    }

    #[test]
    fn bucket_key_keeps_metrics_labels_out_of_the_database_key() {
        assert_eq!(
            bucket_key(ProviderAdmissionScope::Provider, "Anthropic", "ignored"),
            "provider:anthropic"
        );
        assert_eq!(
            bucket_key(
                ProviderAdmissionScope::ProviderModel,
                "OpenAI Proxy",
                "GPT-4.1/Preview"
            ),
            "provider_model:openai_proxy:gpt-4.1_preview"
        );
    }

    #[test]
    fn db_fixed_window_sql_uses_atomic_conditional_claim() {
        assert!(CREATE_WINDOWS_TABLE_SQL.contains("PRIMARY KEY (bucket_key, window_start_ms)"));
        assert!(CREATE_WINDOWS_TABLE_SQL.contains("token_count BIGINT NOT NULL DEFAULT 0"));
        assert!(CREATE_PACING_TABLE_SQL.contains("PRIMARY KEY (bucket_key)"));
        assert!(CREATE_PACING_TABLE_SQL.contains("tat_ms BIGINT NOT NULL DEFAULT 0"));
        assert!(INSERT_WINDOW_SQL.contains("INSERT IGNORE"));
        assert!(INSERT_WINDOW_SQL.contains("request_count, token_count"));
        assert!(INSERT_PACING_SQL.contains("INSERT IGNORE"));
        assert!(CLAIM_PACING_SQL.contains("CASE WHEN tat_ms > ? THEN tat_ms ELSE ? END"));
        assert!(CLAIM_PACING_SQL.contains("tat_ms <= ?"));
        assert!(CLAIM_WINDOW_SLOT_RPM_SQL.contains("request_count = request_count + 1"));
        assert!(CLAIM_WINDOW_SLOT_RPM_SQL.contains("token_count = token_count + ?"));
        assert!(CLAIM_WINDOW_SLOT_RPM_SQL.contains("request_count < ?"));
        assert!(CLAIM_WINDOW_SLOT_TPM_SQL.contains("token_count = token_count + ?"));
        assert!(CLAIM_WINDOW_SLOT_TPM_SQL.contains("token_count + ? <= ?"));
        assert!(CLAIM_WINDOW_SLOT_RPM_TPM_SQL.contains("request_count < ?"));
        assert!(CLAIM_WINDOW_SLOT_RPM_TPM_SQL.contains("token_count + ? <= ?"));
        assert!(CLEANUP_WINDOWS_SQL.contains("DELETE FROM llm_provider_admission_windows"));
        assert!(CLEANUP_WINDOWS_SQL.contains("window_start_ms < ?"));
    }

    #[test]
    fn cleanup_cutoff_keeps_configured_number_of_complete_windows() {
        assert_eq!(cleanup_cutoff_ms(123_456, 60_000, 2), 0);
        assert_eq!(cleanup_cutoff_ms(180_000, 60_000, 2), 60_000);
        assert_eq!(cleanup_cutoff_ms(180_001, 60_000, 2), 60_000);
    }

    #[test]
    fn cleanup_throttle_runs_only_after_interval() {
        assert!(!cleanup_due(1_000, 1_999, 1_000));
        assert!(cleanup_due(1_000, 2_000, 1_000));
        assert!(!cleanup_due(2_000, 1_000, 1_000));
    }

    #[test]
    fn startup_validation_rejects_enabled_mode_without_any_limit() {
        let error = validate_startup_config(&ProviderAdmissionConfig::db_fixed_window(None))
            .expect_err("db mode without rpm/tpm is a deployment error");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn startup_validation_allows_tpm_only_mode() {
        validate_startup_config(&ProviderAdmissionConfig::db_fixed_window_with_tpm(
            None,
            Some(120_000),
        ))
        .expect("tpm-only admission is valid");
    }

    #[test]
    fn startup_validation_rejects_unsupported_mode() {
        let error = validate_startup_config(&ProviderAdmissionConfig::unsupported())
            .expect_err("unsupported mode is a deployment error");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn rejected_limit_classification_distinguishes_rpm_and_tpm() {
        assert_eq!(
            rejected_limit_from_counts(60, 10_000, 1_000, Some(60), Some(120_000)),
            ProviderAdmissionLimit::Rpm
        );
        assert_eq!(
            rejected_limit_from_counts(10, 119_500, 1_000, Some(60), Some(120_000)),
            ProviderAdmissionLimit::Tpm
        );
        assert_eq!(
            rejected_limit_from_counts(60, 119_500, 1_000, Some(60), Some(120_000)),
            ProviderAdmissionLimit::Capacity
        );
    }

    #[test]
    fn calibration_outcome_classifies_missing_close_over_and_under() {
        assert_eq!(calibration_outcome(1_000, 0), "missing_usage");
        assert_eq!(calibration_outcome(1_050, 1_000), "within_10pct");
        assert_eq!(calibration_outcome(1_250, 1_000), "overestimate");
        assert_eq!(calibration_outcome(750, 1_000), "underestimate");
    }

    #[test]
    fn provider_admission_rejection_error_exposes_fail_fast_details() {
        let config = ProviderAdmissionConfig::db_fixed_window_with_tpm(Some(60), Some(120_000));
        let error =
            provider_admission_rejection_error(&config, ProviderAdmissionLimit::Tpm, 42_000, 3_000);

        assert_eq!(error.kind, ErrorKind::RateLimit);
        assert!(error.message.contains("tpm limit"));
        let details: Value =
            serde_json::from_str(error.details_json.as_deref().expect("details json"))
                .expect("valid json details");
        assert_eq!(details["source"], "llm_provider_admission");
        assert_eq!(details["policy"], "fail_fast");
        assert_eq!(details["limit"], "tpm");
        assert_eq!(details["scope"], "provider");
        assert_eq!(details["retry_after_ms"], 42_000);
        assert_eq!(details["estimated_tokens"], 3_000);
        assert_eq!(details["rpm_limit"], 60);
        assert_eq!(details["tpm_limit"], 120_000);
    }

    #[tokio::test]
    async fn disabled_admission_records_disabled_and_allows_without_pool() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());

        let result = admit_llm_provider_request_with_config(
            None,
            "anthropic",
            "claude",
            2_048,
            ProviderAdmissionConfig::disabled(),
        )
        .await;

        assert!(result.is_ok());
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_attempts_total{mode="disabled",outcome="disabled",scope="provider"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_tokens_total{mode="disabled",outcome="disabled",scope="provider"} 2048"#
        ));
    }

    #[tokio::test]
    async fn cleanup_metrics_use_low_cardinality_outcome_labels() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());

        record_cleanup("ok", 42);

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains(r#"astra_llm_provider_admission_cleanup_runs_total{outcome="ok"} 1"#)
        );
        assert!(
            rendered
                .contains(r#"astra_llm_provider_admission_cleanup_rows_total{outcome="ok"} 42"#)
        );
        assert!(!rendered.contains("provider="));
        assert!(!rendered.contains("model="));
        assert!(!rendered.contains("user_id="));
    }

    #[tokio::test]
    async fn calibration_metrics_record_actual_estimate_and_error() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());
        let usage = crate::turn::token_usage::TokenUsage {
            input_tokens: 600,
            cached_input_tokens: 50,
            cache_creation_tokens: 25,
            output_tokens: 325,
        }
        .to_json_map();

        record_llm_provider_admission_calibration(1_250, &usage);

        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_samples_total{outcome="overestimate"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_estimated_tokens_total{outcome="overestimate"} 1250"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_actual_tokens_total{outcome="overestimate"} 1000"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_abs_error_tokens_total{outcome="overestimate"} 250"#
        ));
        assert!(!rendered.contains("provider="));
        assert!(!rendered.contains("model="));
        assert!(!rendered.contains("user_id="));
    }

    #[tokio::test]
    async fn calibration_metrics_record_missing_usage() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());

        record_llm_provider_admission_calibration(2_000, &Map::new());

        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_samples_total{outcome="missing_usage"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_estimated_tokens_total{outcome="missing_usage"} 2000"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_calibration_abs_error_tokens_total{outcome="missing_usage"} 2000"#
        ));
    }

    #[tokio::test]
    async fn db_mode_without_any_limit_fails_closed_before_touching_database() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());

        let result = admit_llm_provider_request_with_config(
            None,
            "anthropic",
            "claude",
            2_048,
            ProviderAdmissionConfig::db_fixed_window(None),
        )
        .await;

        let error = result.expect_err("missing rpm/tpm must fail closed");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_attempts_total{mode="db_fixed_window",outcome="error_fail_closed",scope="provider"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_errors_total{class="misconfigured",mode="db_fixed_window",policy="fail_closed",scope="provider"} 1"#
        ));
    }

    #[tokio::test]
    async fn unsupported_mode_fails_closed_before_touching_database() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());

        let result = admit_llm_provider_request_with_config(
            None,
            "anthropic",
            "claude",
            2_048,
            ProviderAdmissionConfig::unsupported(),
        )
        .await;

        let error = result.expect_err("unsupported mode must fail closed");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_attempts_total{mode="unsupported",outcome="error_fail_closed",scope="provider"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_errors_total{class="unsupported_mode",mode="unsupported",policy="fail_closed",scope="provider"} 1"#
        ));
    }

    #[tokio::test]
    async fn fail_open_allows_missing_database_pool_but_records_policy() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());
        let mut config = ProviderAdmissionConfig::db_fixed_window(Some(600));
        config.fail_open = true;

        let result =
            admit_llm_provider_request_with_config(None, "anthropic", "claude", 2_048, config)
                .await;

        assert!(result.is_ok());
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_attempts_total{mode="db_fixed_window",outcome="error_fail_open",scope="provider"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_errors_total{class="missing_pool",mode="db_fixed_window",policy="fail_open",scope="provider"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_tokens_total{mode="db_fixed_window",outcome="error_fail_open",scope="provider"} 2048"#
        ));
    }
}
