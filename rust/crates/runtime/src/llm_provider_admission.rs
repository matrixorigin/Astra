use std::{
    env,
    sync::{Arc, OnceLock, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use astra_core::{ClassifiedError, ErrorKind, SharedPool};
use astra_turn_core::pipeline_metrics::MetricsRegistry;

const ENV_MODE: &str = "ASTRA_LLM_PROVIDER_ADMISSION_MODE";
const ENV_RPM: &str = "ASTRA_LLM_PROVIDER_ADMISSION_RPM";
const ENV_CAPACITY_RPM: &str = "ASTRA_CAPACITY_PROVIDER_RPM";
const ENV_WINDOW_MS: &str = "ASTRA_LLM_PROVIDER_ADMISSION_WINDOW_MS";
const ENV_SCOPE: &str = "ASTRA_LLM_PROVIDER_ADMISSION_SCOPE";
const ENV_FAIL_OPEN: &str = "ASTRA_LLM_PROVIDER_ADMISSION_FAIL_OPEN";

const DEFAULT_WINDOW_MS: u64 = 60_000;
const MAX_BUCKET_KEY_BYTES: usize = 240;

const METRIC_PROVIDER_ADMISSION_ATTEMPTS_TOTAL: &str =
    "astra_llm_provider_admission_attempts_total";
const METRIC_PROVIDER_ADMISSION_ERRORS_TOTAL: &str = "astra_llm_provider_admission_errors_total";
const METRIC_PROVIDER_ADMISSION_RETRY_AFTER_MS_TOTAL: &str =
    "astra_llm_provider_admission_retry_after_ms_total";

const CREATE_WINDOWS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS llm_provider_admission_windows (
    bucket_key VARCHAR(255) NOT NULL,
    window_start_ms BIGINT NOT NULL,
    request_count BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (bucket_key, window_start_ms),
    INDEX idx_llm_provider_admission_windows_updated (updated_at)
)
"#;

const INSERT_WINDOW_SQL: &str = r#"
INSERT IGNORE INTO llm_provider_admission_windows
    (bucket_key, window_start_ms, request_count)
VALUES (?, ?, 0)
"#;

const CLAIM_WINDOW_SLOT_SQL: &str = r#"
UPDATE llm_provider_admission_windows
SET request_count = request_count + 1,
    updated_at = CURRENT_TIMESTAMP(6)
WHERE bucket_key = ?
  AND window_start_ms = ?
  AND request_count < ?
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderAdmissionConfig {
    mode: ProviderAdmissionMode,
    rpm_limit: Option<u64>,
    window_ms: u64,
    scope: ProviderAdmissionScope,
    fail_open: bool,
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
            window_ms: read_positive_u64(ENV_WINDOW_MS).unwrap_or(DEFAULT_WINDOW_MS),
            scope: env::var(ENV_SCOPE)
                .ok()
                .as_deref()
                .map(parse_scope)
                .unwrap_or(ProviderAdmissionScope::Provider),
            fail_open: read_bool(ENV_FAIL_OPEN).unwrap_or(false),
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            mode: ProviderAdmissionMode::Disabled,
            rpm_limit: None,
            window_ms: DEFAULT_WINDOW_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
        }
    }

    #[cfg(test)]
    fn db_fixed_window(rpm_limit: Option<u64>) -> Self {
        Self {
            mode: ProviderAdmissionMode::DbFixedWindow,
            rpm_limit,
            window_ms: DEFAULT_WINDOW_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
        }
    }

    #[cfg(test)]
    fn unsupported() -> Self {
        Self {
            mode: ProviderAdmissionMode::Unsupported,
            rpm_limit: None,
            window_ms: DEFAULT_WINDOW_MS,
            scope: ProviderAdmissionScope::Provider,
            fail_open: false,
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
    Rejected { retry_after_ms: u64 },
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
        ProviderAdmissionMode::DbFixedWindow if config.rpm_limit.is_none() => {
            Err(ClassifiedError::new(
                ErrorKind::InvalidRequest,
                format!("{ENV_MODE}=db_fixed_window requires {ENV_RPM} or {ENV_CAPACITY_RPM}."),
            ))
        }
        ProviderAdmissionMode::DbFixedWindow => Ok(()),
    }
}

pub(crate) async fn admit_llm_provider_request(
    shared_pool: Option<&SharedPool>,
    provider: &str,
    model: &str,
) -> Result<(), ClassifiedError> {
    let config = ProviderAdmissionConfig::from_env();
    admit_llm_provider_request_with_config(shared_pool, provider, model, config).await
}

async fn admit_llm_provider_request_with_config(
    shared_pool: Option<&SharedPool>,
    provider: &str,
    model: &str,
    config: ProviderAdmissionConfig,
) -> Result<(), ClassifiedError> {
    match config.mode {
        ProviderAdmissionMode::Disabled => {
            record_attempt(&config, "disabled");
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
        ),
        ProviderAdmissionMode::DbFixedWindow => {
            let Some(rpm_limit) = config.rpm_limit else {
                return handle_admission_error(
                    &config,
                    "misconfigured",
                    ClassifiedError::new(
                        ErrorKind::InvalidRequest,
                        format!(
                            "{ENV_MODE}=db_fixed_window requires {ENV_RPM} or {ENV_CAPACITY_RPM}."
                        ),
                    ),
                );
            };
            let Some(shared_pool) = shared_pool else {
                return handle_admission_error(
                    &config,
                    "missing_pool",
                    ClassifiedError::new(
                        ErrorKind::DatabaseError,
                        "LLM provider admission is enabled but no shared database pool is available.",
                    ),
                );
            };
            match db_fixed_window_admit(shared_pool, provider, model, &config, rpm_limit).await {
                Ok(FixedWindowAdmission::Admitted) => {
                    record_attempt(&config, "admitted");
                    Ok(())
                }
                Ok(FixedWindowAdmission::Rejected { retry_after_ms }) => {
                    record_attempt(&config, "rejected");
                    record_retry_after(&config, retry_after_ms);
                    Err(ClassifiedError::new(
                        ErrorKind::RateLimit,
                        format!(
                            "LLM provider admission limit reached for {} scope (limit: {} requests per {}ms). Retry after {}s.",
                            config.scope_label(),
                            rpm_limit,
                            config.window_ms,
                            retry_after_ms.div_ceil(1000)
                        ),
                    ))
                }
                Err(error) => handle_admission_error(&config, "database_error", error),
            }
        }
    }
}

fn handle_admission_error(
    config: &ProviderAdmissionConfig,
    class: &'static str,
    error: ClassifiedError,
) -> Result<(), ClassifiedError> {
    record_error(config, class);
    if config.fail_open {
        record_attempt(config, "error_fail_open");
        Ok(())
    } else {
        record_attempt(config, "error_fail_closed");
        Err(error)
    }
}

async fn db_fixed_window_admit(
    shared_pool: &SharedPool,
    provider: &str,
    model: &str,
    config: &ProviderAdmissionConfig,
    rpm_limit: u64,
) -> Result<FixedWindowAdmission, ClassifiedError> {
    let now_ms = now_epoch_ms();
    let window_start_ms = fixed_window_start_ms(now_ms, config.window_ms);
    let bucket_key = bucket_key(config.scope, provider, model);
    let rpm_limit_i64 = i64::try_from(rpm_limit).unwrap_or(i64::MAX);

    sqlx::query(INSERT_WINDOW_SQL)
        .bind(&bucket_key)
        .bind(window_start_ms)
        .execute(shared_pool.get())
        .await
        .map_err(database_error)?;

    let result = sqlx::query(CLAIM_WINDOW_SLOT_SQL)
        .bind(&bucket_key)
        .bind(window_start_ms)
        .bind(rpm_limit_i64)
        .execute(shared_pool.get())
        .await
        .map_err(database_error)?;

    if result.rows_affected() == 1 {
        Ok(FixedWindowAdmission::Admitted)
    } else {
        Ok(FixedWindowAdmission::Rejected {
            retry_after_ms: retry_after_ms(now_ms, window_start_ms, config.window_ms),
        })
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
        assert_eq!(retry_after_ms(123_456, 120_000, 60_000), 56_544);
        assert_eq!(retry_after_ms(180_000, 120_000, 60_000), 1);
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
        assert!(INSERT_WINDOW_SQL.contains("INSERT IGNORE"));
        assert!(CLAIM_WINDOW_SLOT_SQL.contains("request_count = request_count + 1"));
        assert!(CLAIM_WINDOW_SLOT_SQL.contains("request_count < ?"));
    }

    #[test]
    fn startup_validation_rejects_enabled_mode_without_rpm() {
        let error = validate_startup_config(&ProviderAdmissionConfig::db_fixed_window(None))
            .expect_err("db mode without rpm is a deployment error");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn startup_validation_rejects_unsupported_mode() {
        let error = validate_startup_config(&ProviderAdmissionConfig::unsupported())
            .expect_err("unsupported mode is a deployment error");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
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
            ProviderAdmissionConfig::disabled(),
        )
        .await;

        assert!(result.is_ok());
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_attempts_total{mode="disabled",outcome="disabled",scope="provider"} 1"#
        ));
    }

    #[tokio::test]
    async fn db_mode_without_rpm_fails_closed_before_touching_database() {
        let _guard = metrics_test_lock().lock_owned().await;
        let registry = Arc::new(MetricsRegistry::new());
        set_llm_provider_admission_metrics_registry(registry.clone());

        let result = admit_llm_provider_request_with_config(
            None,
            "anthropic",
            "claude",
            ProviderAdmissionConfig::db_fixed_window(None),
        )
        .await;

        let error = result.expect_err("missing rpm must fail closed");
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
            admit_llm_provider_request_with_config(None, "anthropic", "claude", config).await;

        assert!(result.is_ok());
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_attempts_total{mode="db_fixed_window",outcome="error_fail_open",scope="provider"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_provider_admission_errors_total{class="missing_pool",mode="db_fixed_window",policy="fail_open",scope="provider"} 1"#
        ));
    }
}
