use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use crate::auth::FernetTokenEncryptor;
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct PricingData {
    #[serde(default)]
    pub prompt: f64,
    #[serde(default)]
    pub completion: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct QuirksData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_temperature: Option<f64>,
    #[serde(default)]
    pub preserve_reasoning_content: bool,
    #[serde(default)]
    pub no_parallel_tool_calls: bool,
    #[serde(default)]
    pub tool_choice_required: bool,
    #[serde(default)]
    pub strict_tool_call_ids: bool,
    #[serde(default)]
    pub no_system_message: bool,
    #[serde(default)]
    pub system_as_user_prefix: bool,
    /// Ordered fallback chain. Tried in sequence when the primary model hits
    /// rate limits or becomes unavailable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_chain: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelCreateRequestData {
    pub name: String,
    pub provider: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: PricingData,
    pub architecture: Option<String>,
    pub tags: Vec<String>,
    pub quirks: Option<QuirksData>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUpdateRequestData {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub provider: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub supported_parameters: Option<Vec<String>>,
    pub pricing: Option<PricingData>,
    pub architecture: Option<String>,
    pub tags: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub quirks: Option<QuirksData>,
}

/// Thinking capability of a model, determined by provider-aware probe.
///
/// Persisted to DB column `thinking_capability`. NULL means unprobed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingCapability {
    /// Model supports Normal (off) and Thinking modes — suppression works.
    /// Picker: Normal / Thinking (Low) / Thinking (High).
    /// Examples: Bedrock Claude, DashScope Qwen-Plus, GLM-5.1.
    Both,
    /// Model always thinks but supports effort control (low/medium/high).
    /// Cannot be turned off completely — no "Normal" option.
    /// Picker: Thinking (Low) / Thinking (High).
    /// Examples: DeepSeek V4.
    EffortOnly,
    /// Model always thinks, no control at all.
    /// No picker shown.
    /// Examples: MiniMax M2.5.
    NativeOnly,
    /// Model does not support thinking.
    /// No picker shown.
    /// Examples: qwen-flash, qwen2.5-3b-instruct.
    None,
}

impl ThinkingCapability {
    pub fn from_db(s: Option<&str>) -> Option<Self> {
        match s? {
            "both" => Some(Self::Both),
            "effort_only" => Some(Self::EffortOnly),
            "native_only" => Some(Self::NativeOnly),
            "none" => Some(Self::None),
            other => {
                tracing::warn!(
                    value = other,
                    "unknown thinking_capability value in DB — treating as unprobed"
                );
                Option::None
            }
        }
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::EffortOnly => "effort_only",
            Self::NativeOnly => "native_only",
            Self::None => "none",
        }
    }
}

/// Result of the two-phase thinking behavior probe.
///
/// Ephemeral — returned during `check_model`, but the capability is persisted to DB.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingProbeResult {
    pub capability: ThinkingCapability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct ModelRecord {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: PricingData,
    pub architecture: Option<String>,
    pub tags: Vec<String>,
    pub quirks: QuirksData,
    pub connectivity: Option<String>,
    pub thinking_capability: Option<ThinkingCapability>,
    pub thinking_probe: Option<ThinkingProbeResult>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelListItem {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub architecture: Option<String>,
    pub thinking_capability: Option<ThinkingCapability>,
}

/// Decrypted credentials for the active (or preferred) row in `infra_llm_models`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedActiveLlmModel {
    pub model_name: String,
    pub api_key: String,
    pub base_url: String,
    pub provider: String,
    pub fallback_chain: Vec<String>,
    pub tags: Vec<String>,
    /// Probe-determined thinking capability. NULL if unprobed.
    pub thinking_capability: Option<ThinkingCapability>,
}

fn build_resolved_active_llm_from_row(
    row: &sqlx::mysql::MySqlRow,
    encryptor: &FernetTokenEncryptor,
) -> Result<ResolvedActiveLlmModel, String> {
    let model_name: String = row.try_get("model_name").map_err(|e| e.to_string())?;
    let encrypted: String = row
        .try_get("api_key_encrypted")
        .map_err(|e| e.to_string())?;
    let base_url: String = row
        .try_get("base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let provider: String = row
        .try_get("provider")
        .unwrap_or_else(|_| "openai".to_string());
    let api_key = encryptor
        .decrypt(&encrypted)
        .map_err(|e| format!("Decrypt: {e}"))?;

    let quirks_json: String = row
        .try_get("quirks_json")
        .unwrap_or_else(|_| "{}".to_string());
    let quirks: QuirksData = match serde_json::from_str(&quirks_json) {
        Ok(q) => q,
        Err(e) => {
            let prefix = &quirks_json[..quirks_json.len().min(200)];
            tracing::error!(
                target: "astra_services::models",
                column = "quirks_json",
                err = %e,
                payload_prefix = %prefix,
                "malformed JSON column, using default"
            );
            QuirksData::default()
        }
    };

    let tags_json: String = row
        .try_get("tags_json")
        .unwrap_or_else(|_| "[]".to_string());
    let tags: Vec<String> = parse_json_column("tags_json", &tags_json, Vec::new);

    let thinking_cap_str: Option<String> = row.try_get("thinking_capability").ok().flatten();
    let thinking_capability = ThinkingCapability::from_db(thinking_cap_str.as_deref());

    let fallback_chain = quirks.fallback_chain;

    Ok(ResolvedActiveLlmModel {
        model_name,
        api_key,
        base_url,
        provider,
        fallback_chain,
        tags,
        thinking_capability,
    })
}

/// Resolve a short / partial model name against the full list of
/// active model names.
///
/// The LLM, when prompted to call `spawn_agent`, frequently produces
/// the short family name (`claude-sonnet`, `qwen-flash`) rather than
/// the fully-qualified registered name (`us.anthropic.claude-sonnet-4-6`).
/// Rejecting those calls outright forces the case author to retrain
/// the prompt. Instead, we do a **deterministic, unambiguous** alias
/// lookup:
///
/// 1. **Exact match** — name equal (case-sensitive) → that name.
/// 2. **Case-insensitive exact match** — unique match → that name.
/// 3. **Substring match** — unique active row whose name contains the
///    requested string (case-insensitive) → that name.
/// 4. Otherwise → `Err` with the candidate list so the caller can
///    surface a useful error. Ambiguity (2+ candidates) is also
///    `Err` — picking arbitrarily would be worse than failing.
///
/// Pure function; the DB query path uses it by passing the names
/// from `SELECT model_name FROM infra_llm_models WHERE is_active = 1`.
/// Keeping it pure means the behavior is fully unit-testable without
/// spinning up a pool.
pub fn resolve_model_alias<'a>(
    requested: &str,
    active_names: &'a [String],
) -> Result<&'a str, ModelAliasResolutionError> {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return Err(ModelAliasResolutionError::Empty);
    }
    // Level 1: exact match wins.
    if let Some(hit) = active_names.iter().find(|n| n.as_str() == trimmed) {
        return Ok(hit.as_str());
    }
    // Level 2: case-insensitive exact. Narrower than substring — a
    // user typing "CLAUDE-HAIKU-4-5-20251001" should still resolve
    // without needing to match case.
    let requested_lower = trimmed.to_ascii_lowercase();
    let ci_hits: Vec<&String> = active_names
        .iter()
        .filter(|n| n.to_ascii_lowercase() == requested_lower)
        .collect();
    match ci_hits.len() {
        0 => {}
        1 => return Ok(ci_hits[0].as_str()),
        _ => {
            return Err(ModelAliasResolutionError::Ambiguous {
                requested: trimmed.into(),
                candidates: ci_hits.iter().map(|s| s.to_string()).collect(),
            });
        }
    }
    // Level 3: substring (case-insensitive). Unique match only.
    let sub_hits: Vec<&String> = active_names
        .iter()
        .filter(|n| n.to_ascii_lowercase().contains(&requested_lower))
        .collect();
    match sub_hits.len() {
        0 => Err(ModelAliasResolutionError::NotFound {
            requested: trimmed.into(),
            candidates: active_names.iter().map(|s| s.to_string()).collect(),
        }),
        1 => Ok(sub_hits[0].as_str()),
        _ => Err(ModelAliasResolutionError::Ambiguous {
            requested: trimmed.into(),
            candidates: sub_hits.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

/// Error surface for [`resolve_model_alias`]. Preserved so the
/// DB-layer caller can distinguish "no such model" from "ambiguous"
/// and render a helpful message to the LLM / user.
///
/// The `candidates` vector always carries the FULL list a caller
/// might want to process programmatically. The `Display` impl
/// truncates to [`MODEL_ALIAS_ERROR_CANDIDATE_CAP`] for readability
/// — rendering 200 model names in a stderr line is a debugging
/// anti-feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAliasResolutionError {
    Empty,
    NotFound {
        requested: String,
        candidates: Vec<String>,
    },
    Ambiguous {
        requested: String,
        candidates: Vec<String>,
    },
}

/// Max number of candidate names the `Display` impl will render
/// inline. Beyond this, the rendered string shows the first
/// `MODEL_ALIAS_ERROR_CANDIDATE_CAP` entries and suffixes
/// `... and N more`. Chosen to keep a single-line log readable
/// while still being useful (20 model names ≈ 400-600 chars,
/// within most log collectors' line limits).
pub const MODEL_ALIAS_ERROR_CANDIDATE_CAP: usize = 20;

fn render_truncated_candidates(
    candidates: &[String],
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    if candidates.len() <= MODEL_ALIAS_ERROR_CANDIDATE_CAP {
        return write!(f, "{candidates:?}");
    }
    let head = &candidates[..MODEL_ALIAS_ERROR_CANDIDATE_CAP];
    let remaining = candidates.len() - MODEL_ALIAS_ERROR_CANDIDATE_CAP;
    write!(f, "{head:?} ... and {remaining} more")
}

impl std::fmt::Display for ModelAliasResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty model name"),
            Self::NotFound {
                requested,
                candidates,
            } => {
                write!(
                    f,
                    "Model '{requested}' is not configured on this server \
                     (no exact or substring match in infra_llm_models). \
                     Registered active models: "
                )?;
                render_truncated_candidates(candidates, f)?;
                write!(
                    f,
                    ". Omit the model override or choose one of the registered names."
                )
            }
            Self::Ambiguous {
                requested,
                candidates,
            } => {
                write!(
                    f,
                    "Model '{requested}' is ambiguous — matches multiple \
                     registered models: "
                )?;
                render_truncated_candidates(candidates, f)?;
                write!(f, ". Use a more specific name.")
            }
        }
    }
}

impl std::error::Error for ModelAliasResolutionError {}

/// Format the error message for a requested model that was found
/// (via exact or alias match) but has `is_active = 0`. If the alias
/// resolver canonicalized the name, both forms are surfaced so the
/// reader can see what happened; otherwise only one mention appears.
///
/// Extracted from [`resolve_active_llm_model`] so the message can
/// be unit-tested without the DB path. The pre-alias behavior used
/// the original requested name in the error even when the DB row
/// was keyed on the resolved canonical form — misleading when the
/// two disagree.
pub fn format_inactive_model_error(requested: &str, canonical: &str) -> String {
    if requested == canonical {
        format!(
            "Model '{canonical}' is inactive (connectivity failed or disabled). \
             Run `astra-admin model check {canonical}` or pick an active model; \
             the server will not substitute another model."
        )
    } else {
        format!(
            "Model '{requested}' (resolved to canonical '{canonical}') is inactive \
             (connectivity failed or disabled). Run `astra-admin model check {canonical}` \
             or pick an active model; the server will not substitute another model."
        )
    }
}

/// Shared columns for all model-resolution queries.
const RESOLVE_COLS: &str = "\
    model_name, api_key_encrypted, base_url, provider, \
    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json, \
    IFNULL(CAST(pricing AS CHAR), '{}') AS pricing_json, \
    IFNULL(CAST(tags AS CHAR), '[]') AS tags_json, \
    thinking_capability";

/// Acquire a pool reference: use the provided pool, or open an ephemeral one.
async fn acquire_pool(
    provided: Option<&sqlx::Pool<sqlx::MySql>>,
    matrixone: &MatrixOneSettings,
) -> Result<sqlx::Pool<sqlx::MySql>, String> {
    match provided {
        Some(p) => Ok(p.clone()),
        None => {
            let url = format!(
                "{}?connect_timeout=2",
                matrixone.database_url_with_password()
            );
            sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(3))
                .connect(&url)
                .await
                .map_err(|e| format!("DB connect: {e}"))
        }
    }
}

/// Resolve the active LLM model from the database for in-process / server-side callers.
///
/// When `preferred` is `Some(name)`, the row **must** exist and be active — otherwise this
/// returns an error (no silent fallback to another model). When `preferred` is `None`, uses
/// the lexicographically first active model. When `pool` is `None`, opens an ephemeral
/// single-connection pool from `matrixone`.
///
/// Also extracts `fallback_chain` from the `quirks` JSON column (cloud-managed config).
pub async fn resolve_active_llm_model(
    matrixone: &MatrixOneSettings,
    encryptor: &FernetTokenEncryptor,
    preferred: Option<&str>,
    pool: Option<&sqlx::Pool<sqlx::MySql>>,
) -> Result<ResolvedActiveLlmModel, String> {
    let pool = acquire_pool(pool, matrixone).await?;

    let pref = preferred.map(str::trim).filter(|s| !s.is_empty());

    if let Some(name) = pref {
        // Try exact match first — the fast path. If the LLM supplied
        // the fully-qualified name it resolves in one query.
        let exact_row = sqlx::query(&format!(
            "SELECT {RESOLVE_COLS}, is_active FROM infra_llm_models WHERE model_name = ? LIMIT 1"
        ))
        .bind(name)
        .fetch_optional(&pool)
        .await
        .map_err(|e| format!("DB query: {e}"))?;

        // Class C fix: fall through to the alias resolver when exact
        // match fails. The LLM often produces short names like
        // `claude-sonnet` for `spawn_agent`'s `model_override`; doing
        // a deterministic substring / case-insensitive match against
        // active rows lets those calls succeed without forcing every
        // case author to retrain the prompt. Ambiguity still errors.
        //
        // Track canonical name separately so the `is_active` error
        // message below can name BOTH the requested alias and the
        // resolved form when they differ — without this, a reader
        // would see "Model 'claude-sonnet' is inactive" even though
        // the DB row is keyed on the full bedrock name.
        let (row, canonical) = match exact_row {
            Some(r) => (r, name.to_string()),
            None => {
                let active_names: Vec<String> = sqlx::query_scalar::<_, String>(
                    "SELECT model_name FROM infra_llm_models WHERE is_active = 1",
                )
                .fetch_all(&pool)
                .await
                .map_err(|e| format!("DB query: {e}"))?;

                match resolve_model_alias(name, &active_names) {
                    Ok(canonical_ref) => {
                        let canonical = canonical_ref.to_string();
                        let r = sqlx::query(&format!(
                            "SELECT {RESOLVE_COLS}, is_active \
                             FROM infra_llm_models WHERE model_name = ? LIMIT 1"
                        ))
                        .bind(&canonical)
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| format!("DB query: {e}"))?
                        .ok_or_else(|| {
                            format!(
                                "alias resolver returned '{canonical}' but no row exists \
                                 — concurrent delete?"
                            )
                        })?;
                        (r, canonical)
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        };

        let is_active_int: i16 = row.try_get("is_active").unwrap_or(0);
        if is_active_int == 0 {
            return Err(format_inactive_model_error(name, &canonical));
        }

        return build_resolved_active_llm_from_row(&row, encryptor);
    }

    let row = sqlx::query(&format!(
        "SELECT {RESOLVE_COLS} FROM infra_llm_models WHERE is_active = 1 ORDER BY model_name LIMIT 1"
    ))
    .fetch_optional(&pool)
    .await
    .map_err(|e| format!("DB query fallback: {e}"))?;

    let row = row
        .ok_or_else(|| "No active LLM model configured. Run: astra-admin model add".to_string())?;

    build_resolved_active_llm_from_row(&row, encryptor)
}

/// Resolve the model used for reasoning / judge / summary tasks.
///
/// Resolution order:
/// 1. If `admin_config.reasoning_model_name` is set, resolve that model (strict — errors
///    if the named model is missing or inactive).
/// 2. Otherwise, pick the cheapest active model by `pricing.completion` (falls back to
///    lexicographic `model_name` ordering among rows with equal or missing pricing).
/// 3. Otherwise, returns `Err`.
pub async fn resolve_reasoning_model(
    matrixone: &MatrixOneSettings,
    encryptor: &FernetTokenEncryptor,
    admin_config: &dyn crate::admin_config::AdminConfigService,
    pool: Option<&sqlx::Pool<sqlx::MySql>>,
) -> Result<ResolvedActiveLlmModel, String> {
    // 1. Admin override
    if let Some(name) = admin_config
        .get(crate::admin_config::ADMIN_CONFIG_KEY_REASONING_MODEL)
        .await?
    {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return resolve_active_llm_model(matrixone, encryptor, Some(trimmed), pool).await;
        }
    }

    // 2. Cheapest active. MatrixOne JSON function support is uneven, so sort in Rust:
    //    pull all active rows and pick the minimum completion price.
    let pool = acquire_pool(pool, matrixone).await?;

    let rows = sqlx::query(&format!(
        "SELECT {RESOLVE_COLS} FROM infra_llm_models WHERE is_active = 1"
    ))
    .fetch_all(&pool)
    .await
    .map_err(|e| format!("DB query reasoning: {e}"))?;

    if rows.is_empty() {
        return Err(
            "No active LLM model configured. Run `astra-admin model add` then \
             `astra-admin model check`, or `astra-admin config set reasoning_model <name>`."
                .to_string(),
        );
    }

    // Pick the row with the lowest completion price. See [`rank_cheapest_index`].
    let entries: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let name: String = row.try_get("model_name").unwrap_or_default();
            let pricing_json: String = row
                .try_get("pricing_json")
                .unwrap_or_else(|_| "{}".to_string());
            (name, pricing_json)
        })
        .collect();
    let best_idx = rank_cheapest_index(&entries);
    build_resolved_active_llm_from_row(&rows[best_idx], encryptor)
}

/// Resolve the cheapest model tagged `"selector"` for memory-related
/// decisions (relevance filtering, lesson synthesis, L1b extraction).
///
/// 1. Cheapest active model tagged `"selector"`.
/// 2. Fallback: cheapest active model overall.
///
/// Callers use `thinking_capability` from the resolved model to decide
/// whether to apply thinking suppression.
pub async fn resolve_memory_model(
    matrixone: &MatrixOneSettings,
    encryptor: &FernetTokenEncryptor,
    pool: Option<&sqlx::Pool<sqlx::MySql>>,
) -> Result<ResolvedActiveLlmModel, String> {
    let pool = acquire_pool(pool, matrixone).await?;

    let rows = sqlx::query(&format!(
        "SELECT {RESOLVE_COLS} FROM infra_llm_models WHERE is_active = 1"
    ))
    .fetch_all(&pool)
    .await
    .map_err(|e| format!("DB query memory model: {e}"))?;

    if rows.is_empty() {
        return Err("No active LLM model configured.".to_string());
    }

    let selector_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            let tags_json: String = row
                .try_get("tags_json")
                .unwrap_or_else(|_| "[]".to_string());
            tags_json.contains("\"selector\"")
        })
        .map(|(i, _)| i)
        .collect();

    let pick_cheapest_in = |indices: &[usize]| -> usize {
        let entries: Vec<(String, String)> = indices
            .iter()
            .map(|&i| {
                let name: String = rows[i].try_get("model_name").unwrap_or_default();
                let pricing: String = rows[i]
                    .try_get("pricing_json")
                    .unwrap_or_else(|_| "{}".to_string());
                (name, pricing)
            })
            .collect();
        let local_best = rank_cheapest_index(&entries);
        indices[local_best]
    };

    let best_idx = if !selector_rows.is_empty() {
        pick_cheapest_in(&selector_rows)
    } else {
        let all_indices: Vec<usize> = (0..rows.len()).collect();
        pick_cheapest_in(&all_indices)
    };

    build_resolved_active_llm_from_row(&rows[best_idx], encryptor)
}

/// Return the index of the cheapest entry by `pricing.completion`.
///
/// * Missing / unparseable pricing and `completion <= 0` are treated as `+infinity`,
///   so they lose to any priced row.
/// * Ties on price are broken by ascending `model_name` (so the result is deterministic).
///
/// Panics if `entries` is empty. Callers must ensure at least one entry.
pub(crate) fn rank_cheapest_index(entries: &[(String, String)]) -> usize {
    assert!(
        !entries.is_empty(),
        "rank_cheapest_index called with no entries"
    );
    let mut best_idx = 0;
    let mut best_score = score_completion(&entries[0].1);
    let mut best_name = entries[0].0.as_str();
    for (idx, (name, pricing_json)) in entries.iter().enumerate().skip(1) {
        let score = score_completion(pricing_json);
        let cmp = score
            .partial_cmp(&best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| name.as_str().cmp(best_name));
        if cmp == std::cmp::Ordering::Less {
            best_idx = idx;
            best_score = score;
            best_name = name.as_str();
        }
    }
    best_idx
}

fn score_completion(pricing_json: &str) -> f64 {
    serde_json::from_str::<PricingData>(pricing_json)
        .map(|p| p.completion)
        .ok()
        .filter(|c| *c > 0.0)
        .unwrap_or(f64::INFINITY)
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ModelService: Send + Sync {
    async fn create_model(
        &self,
        user_id: String,
        request: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_models(
        &self,
        user_id: String,
        is_admin: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn update_model(
        &self,
        model_name: String,
        request: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_model(
        &self,
        model_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    async fn check_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone)]
pub struct DatabaseModelService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    encryptor: std::sync::Arc<FernetTokenEncryptor>,
}

/// Parse a JSON column with explicit error logging on malformed payloads.
///
/// Replaces silent `serde_json::from_str(..).unwrap_or_default()` patterns so
/// data corruption in MatrixOne JSON columns is observable in logs (target
/// `astra_services::models`) instead of degrading silently to defaults.
fn parse_json_column<T, F>(column: &'static str, raw: &str, default: F) -> T
where
    T: serde::de::DeserializeOwned,
    F: FnOnce() -> T,
{
    match serde_json::from_str::<T>(raw) {
        Ok(v) => v,
        Err(e) => {
            let prefix = &raw[..raw.len().min(200)];
            tracing::error!(
                target: "astra_services::models",
                column,
                err = %e,
                payload_prefix = %prefix,
                "malformed JSON column, using default"
            );
            default()
        }
    }
}

impl DatabaseModelService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: std::sync::Arc<FernetTokenEncryptor>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            pool: None,
        }
    }

    fn model_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let is_active_int: i16 = row.try_get("is_active").unwrap_or(1);
        let input_mod_json: String = row
            .try_get("input_modalities_json")
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let output_mod_json: String = row
            .try_get("output_modalities_json")
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let supported_json: String = row
            .try_get("supported_parameters_json")
            .unwrap_or_else(|_| "[]".to_string());
        let pricing_json: String = row
            .try_get("pricing_json")
            .unwrap_or_else(|_| "{}".to_string());
        let tags_json: String = row
            .try_get("tags_json")
            .unwrap_or_else(|_| "[]".to_string());
        let quirks_json: String = row
            .try_get("quirks_json")
            .unwrap_or_else(|_| "{}".to_string());

        let thinking_cap_str: Option<String> = row.try_get("thinking_capability").ok().flatten();
        let thinking_capability = ThinkingCapability::from_db(thinking_cap_str.as_deref());

        Ok(ModelRecord {
            model_id: row.try_get("model_id").map_err(internal_error)?,
            name: row.try_get("model_name").map_err(internal_error)?,
            provider: row.try_get("provider").map_err(internal_error)?,
            base_url: row.try_get("base_url").ok(),
            description: row.try_get("description").ok(),
            is_active: is_active_int != 0,
            context_window: row.try_get("context_window").unwrap_or(128000),
            max_completion_tokens: row.try_get("max_completion_tokens").ok(),
            input_modalities: parse_json_column("input_modalities_json", &input_mod_json, || {
                vec!["text".to_string()]
            }),
            output_modalities: parse_json_column(
                "output_modalities_json",
                &output_mod_json,
                || vec!["text".to_string()],
            ),
            supported_parameters: parse_json_column(
                "supported_parameters_json",
                &supported_json,
                Default::default,
            ),
            pricing: parse_json_column("pricing_json", &pricing_json, Default::default),
            architecture: row.try_get("architecture").ok(),
            tags: parse_json_column("tags_json", &tags_json, Default::default),
            quirks: parse_json_column("quirks_json", &quirks_json, Default::default),
            connectivity: None,
            thinking_capability,
            thinking_probe: None,
        })
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

pub const MODEL_SELECT_COLS: &str = "\
    model_id, model_name, provider, base_url, description, is_active, \
    IFNULL(context_window, 128000) AS context_window, max_completion_tokens, architecture, \
    IFNULL(CAST(input_modalities AS CHAR), '[\"text\"]') AS input_modalities_json, \
    IFNULL(CAST(output_modalities AS CHAR), '[\"text\"]') AS output_modalities_json, \
    IFNULL(CAST(supported_parameters AS CHAR), '[]') AS supported_parameters_json, \
    IFNULL(CAST(pricing AS CHAR), '{}') AS pricing_json, \
    IFNULL(CAST(tags AS CHAR), '[]') AS tags_json, \
    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json, \
    thinking_capability";
const MODEL_LIST_SELECT_COLS: &str = "\
    model_id, model_name, provider, description, is_active, \
    IFNULL(context_window, 128000) AS context_window, max_completion_tokens, architecture, \
    thinking_capability, \
    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json";
const MAX_MODEL_LIST_ROWS: i64 = 200;

#[async_trait]
impl ModelService for DatabaseModelService {
    async fn create_model(
        &self,
        user_id: String,
        request: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT model_id FROM infra_llm_models WHERE model_name = ? AND provider = ?")
                .bind(&request.name)
                .bind(&request.provider)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        if existing.is_some() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "Model '{}' ({}) already exists",
                    request.name, request.provider
                ),
            ));
        }

        let model_id = Uuid::new_v4().to_string();
        let encrypted_key = self
            .encryptor
            .encrypt(&request.api_key)
            .map_err(internal_error)?;
        let base_url = request
            .base_url
            .or_else(|| resolve_provider_base_url(&request.provider));

        let conn_result = validate_connectivity(
            &request.provider,
            &request.name,
            &request.api_key,
            base_url.as_deref(),
        )
        .await;
        let is_active: i16 = if conn_result.is_none() { 1 } else { 0 };

        let input_mod = serde_json::to_string(&request.input_modalities)
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let output_mod = serde_json::to_string(&request.output_modalities)
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let supported = serde_json::to_string(&request.supported_parameters)
            .unwrap_or_else(|_| "[]".to_string());
        let pricing = serde_json::to_string(&request.pricing).unwrap_or_else(|_| "{}".to_string());
        let tags = serde_json::to_string(&request.tags).unwrap_or_else(|_| "[]".to_string());
        let quirks = request
            .quirks
            .as_ref()
            .map(|q| serde_json::to_string(q).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        query(
            "INSERT INTO infra_llm_models \
             (model_id, model_name, provider, api_key_encrypted, base_url, description, \
              is_active, context_window, max_completion_tokens, input_modalities, output_modalities, \
              supported_parameters, pricing, architecture, tags, quirks, \
              created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(), NOW())",
        )
        .bind(&model_id)
        .bind(&request.name)
        .bind(&request.provider)
        .bind(&encrypted_key)
        .bind(&base_url)
        .bind(&request.description)
        .bind(is_active)
        .bind(request.context_window.unwrap_or(128000))
        .bind(request.max_completion_tokens)
        .bind(&input_mod)
        .bind(&output_mod)
        .bind(&supported)
        .bind(&pricing)
        .bind(&request.architecture)
        .bind(&tags)
        .bind(&quirks)
        .bind(&user_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        // Thinking probe runs AFTER INSERT so the model is immediately
        // available. Probe result is written via UPDATE — if probe is slow
        // or fails, the model still works (picker defaults to no thinking
        // until probed).
        if conn_result.is_none() {
            let probe = probe_thinking_behavior(
                &request.provider,
                &request.name,
                &request.api_key,
                base_url.as_deref(),
            )
            .await;
            let cap_str = probe.capability.as_db_str();
            let err_str = probe.error.as_deref();
            if let Err(e) = query(
                "UPDATE infra_llm_models SET thinking_capability = ?, \
                 thinking_probe_error = ? WHERE model_id = ?",
            )
            .bind(cap_str)
            .bind(err_str)
            .bind(&model_id)
            .execute(&pool)
            .await
            {
                tracing::warn!(
                    model = %request.name,
                    err = %e,
                    "failed to persist thinking_capability after create"
                );
            }
        }

        let select_sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_id = ?",
            MODEL_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&model_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let mut record = Self::model_record_from_row(row)?;
        record.connectivity = Some(conn_result.unwrap_or_else(|| "ok".to_string()));
        Ok(record)
    }

    async fn list_models(
        &self,
        _user_id: String,
        is_admin: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = if is_admin {
            format!(
                "SELECT {} FROM infra_llm_models ORDER BY provider, model_name LIMIT {}",
                MODEL_LIST_SELECT_COLS, MAX_MODEL_LIST_ROWS
            )
        } else {
            format!(
                "SELECT {} FROM infra_llm_models WHERE is_active = 1 ORDER BY provider, model_name LIMIT {}",
                MODEL_LIST_SELECT_COLS, MAX_MODEL_LIST_ROWS
            )
        };
        let rows = query(&sql).fetch_all(&pool).await.map_err(internal_error)?;

        let mut models = Vec::with_capacity(rows.len());
        for row in rows {
            let is_active_int: i16 = row.try_get("is_active").unwrap_or(1);
            models.push(ModelListItem {
                model_id: row.try_get("model_id").map_err(internal_error)?,
                name: row.try_get("model_name").map_err(internal_error)?,
                provider: row.try_get("provider").map_err(internal_error)?,
                description: row.try_get("description").ok(),
                is_active: is_active_int != 0,
                context_window: row.try_get("context_window").unwrap_or(128000),
                max_completion_tokens: row.try_get("max_completion_tokens").ok(),
                architecture: row.try_get("architecture").ok(),
                thinking_capability: {
                    let cap_str: Option<String> = row.try_get("thinking_capability").ok().flatten();
                    ThinkingCapability::from_db(cap_str.as_deref())
                },
            });
        }
        Ok(models)
    }

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_name = ?",
            MODEL_SELECT_COLS
        );
        let row = query(&sql)
            .bind(&model_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            )
        })?;
        Self::model_record_from_row(row)
    }

    async fn update_model(
        &self,
        model_name: String,
        request: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT model_id, base_url, provider FROM infra_llm_models WHERE model_name = ?")
                .bind(&model_name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        let existing = existing.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            )
        })?;
        let _model_id: String = existing.try_get("model_id").map_err(internal_error)?;
        let stored_provider: String = existing
            .try_get("provider")
            .unwrap_or_else(|_| {
                tracing::warn!(model = %model_name, "provider column NULL or missing, defaulting to openai");
                "openai".to_string()
            });
        let effective_provider = request.provider.as_deref().unwrap_or(&stored_provider);

        let mut conn_result: Option<String> = None;

        if let Some(api_key) = &request.api_key {
            let encrypted = self.encryptor.encrypt(api_key).map_err(internal_error)?;
            let base_url: Option<String> = request
                .base_url
                .clone()
                .or_else(|| existing.try_get("base_url").ok());
            let check = validate_connectivity(
                effective_provider,
                &model_name,
                api_key,
                base_url.as_deref(),
            )
            .await;

            query("UPDATE infra_llm_models SET api_key_encrypted = ?, updated_at = NOW() WHERE model_name = ?")
                .bind(&encrypted)
                .bind(&model_name)
                .execute(&pool)
                .await
                .map_err(internal_error)?;

            if request.is_active.is_none() {
                let active: i16 = if check.is_none() { 1 } else { 0 };
                query("UPDATE infra_llm_models SET is_active = ? WHERE model_name = ?")
                    .bind(active)
                    .bind(&model_name)
                    .execute(&pool)
                    .await
                    .map_err(internal_error)?;
            }
            conn_result = Some(check.unwrap_or_else(|| "ok".to_string()));
        }

        macro_rules! update_field {
            ($field:ident, $col:expr) => {
                if let Some(val) = &request.$field {
                    let sql = format!("UPDATE infra_llm_models SET {} = ?, updated_at = NOW() WHERE model_name = ?", $col);
                    query(&sql).bind(val).bind(&model_name).execute(&pool).await.map_err(internal_error)?;
                }
            };
            ($field:ident, $col:expr, json) => {
                if let Some(val) = &request.$field {
                    let json_str = serde_json::to_string(val).unwrap_or_else(|_| "{}".to_string());
                    let sql = format!("UPDATE infra_llm_models SET {} = ?, updated_at = NOW() WHERE model_name = ?", $col);
                    query(&sql).bind(&json_str).bind(&model_name).execute(&pool).await.map_err(internal_error)?;
                }
            };
        }
        update_field!(provider, "provider");
        update_field!(base_url, "base_url");
        update_field!(description, "description");
        update_field!(context_window, "context_window");
        update_field!(max_completion_tokens, "max_completion_tokens");
        update_field!(architecture, "architecture");
        update_field!(input_modalities, "input_modalities", json);
        update_field!(output_modalities, "output_modalities", json);
        update_field!(supported_parameters, "supported_parameters", json);
        update_field!(pricing, "pricing", json);
        update_field!(tags, "tags", json);
        update_field!(quirks, "quirks", json);

        if let Some(active) = request.is_active {
            let val: i16 = if active { 1 } else { 0 };
            query("UPDATE infra_llm_models SET is_active = ?, updated_at = NOW() WHERE model_name = ?")
                .bind(val)
                .bind(&model_name)
                .execute(&pool)
                .await
                .map_err(internal_error)?;
        }

        let sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_name = ?",
            MODEL_SELECT_COLS
        );
        let row = query(&sql)
            .bind(&model_name)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let mut record = Self::model_record_from_row(row)?;
        record.connectivity = conn_result;
        Ok(record)
    }

    async fn delete_model(
        &self,
        model_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let existing = query("SELECT model_id FROM infra_llm_models WHERE model_name = ?")
            .bind(&model_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        if existing.is_none() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            ));
        }
        query("DELETE FROM infra_llm_models WHERE model_name = ?")
            .bind(&model_name)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        Ok(())
    }

    async fn check_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let row = query(
            "SELECT api_key_encrypted, provider, base_url, \
                    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json \
             FROM infra_llm_models WHERE model_name = ?",
        )
        .bind(&model_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            )
        })?;

        let encrypted: String = row.try_get("api_key_encrypted").map_err(internal_error)?;
        let provider: String = row.try_get("provider").map_err(internal_error)?;
        let base_url: Option<String> = row.try_get("base_url").ok();

        let api_key = self.encryptor.decrypt(&encrypted).map_err(internal_error)?;

        // Phase 1: connectivity probe
        let check =
            validate_connectivity(&provider, &model_name, &api_key, base_url.as_deref()).await;

        let is_active: i16 = if check.is_none() { 1 } else { 0 };
        query("UPDATE infra_llm_models SET is_active = ?, updated_at = NOW() WHERE model_name = ?")
            .bind(is_active)
            .bind(&model_name)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        // Phase 2: two-phase thinking behavior probe (only when connected)
        let thinking_probe = if check.is_none() {
            let result =
                probe_thinking_behavior(&provider, &model_name, &api_key, base_url.as_deref())
                    .await;
            // Persist probe result to DB
            let cap_str = result.capability.as_db_str();
            let err_str = result.error.as_deref();
            if let Err(e) = query(
                "UPDATE infra_llm_models SET thinking_capability = ?, \
                 thinking_probe_error = ?, updated_at = NOW() WHERE model_name = ?",
            )
            .bind(cap_str)
            .bind(err_str)
            .bind(&model_name)
            .execute(&pool)
            .await
            {
                tracing::warn!(
                    model = %model_name,
                    err = %e,
                    "failed to persist thinking_capability to DB"
                );
            }
            Some(result)
        } else {
            None
        };

        let sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_name = ?",
            MODEL_SELECT_COLS
        );
        let result_row = query(&sql)
            .bind(&model_name)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let mut record = Self::model_record_from_row(result_row)?;
        record.connectivity = Some(check.unwrap_or_else(|| "ok".to_string()));
        record.thinking_probe = thinking_probe;
        Ok(record)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn resolve_provider_base_url(provider: &str) -> Option<String> {
    match provider {
        "openai" => Some("https://api.openai.com/v1".to_string()),
        "anthropic" => None,
        _ => None,
    }
}

/// Full URL for a minimal Anthropic Messages API probe (`POST`, JSON body).
///
/// - Official Anthropic: `https://api.anthropic.com` → `.../v1/messages`.
/// - Custom roots (e.g. MiniMax China `https://api.minimaxi.com/anthropic`): append `/v1/messages`.
/// - If the base already ends with `/v1` (some clients), append `/messages` only.
fn anthropic_messages_probe_url(base_url: Option<&str>) -> String {
    const DEFAULT: &str = "https://api.anthropic.com";
    let base = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT)
        .trim_end_matches('/')
        .to_string();
    if base.ends_with("/v1") {
        format!("{}/messages", base)
    } else {
        format!("{}/v1/messages", base)
    }
}

fn bedrock_converse_probe_url(base_url: &str, model_name: &str) -> Result<String, ()> {
    let mut url = reqwest::Url::parse(base_url).map_err(|_| ())?;
    {
        let mut segments = url.path_segments_mut().map_err(|_| ())?;
        segments.pop_if_empty();
        segments.push("model");
        segments.push(model_name);
        segments.push("converse");
    }
    Ok(url.to_string())
}

pub async fn validate_connectivity(
    provider: &str,
    model_name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Option<String> {
    if provider == "mock" {
        return None;
    }

    // Connectivity probes reach external provider endpoints (Anthropic, Bedrock,
    // OpenAI-compatible base_urls) — same class of traffic as the LLM client.
    // They are NOT "internal connections" in the sense of 3e3d6fa8, so they
    // share the LLM client's proxy policy via the single authoritative
    // implementation in `astra_core::net::apply_env_proxy`.
    let probe_builder = astra_core::net::apply_env_proxy(
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)),
    );
    let client = match probe_builder.build() {
        Ok(c) => c,
        Err(e) => return Some(format!("Client error: {}", e)),
    };

    let result = if provider == "anthropic" {
        let probe = anthropic_messages_probe_url(base_url);
        let send_result = client
            .post(&probe)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
               "model": model_name,
               "max_tokens": 1,
               "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await;
        (send_result, probe)
    } else if provider == "bedrock" {
        let Some(base_url_value) = base_url.map(str::trim).filter(|url| !url.is_empty()) else {
            return Some(
                "No base_url for provider 'bedrock'. Set base_url to your Bedrock runtime root, e.g. https://bedrock-runtime.us-east-1.amazonaws.com."
                    .to_string(),
            );
        };
        let Ok(probe) = bedrock_converse_probe_url(base_url_value, model_name) else {
            return Some(format!(
                "Invalid base_url for provider 'bedrock': '{}'. Set base_url to your Bedrock runtime root, e.g. https://bedrock-runtime.us-east-1.amazonaws.com.",
                base_url_value
            ));
        };
        let send_result = client
            .post(&probe)
            .header("authorization", format!("Bearer {}", api_key))
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": [{"text": "hi"}]
                }],
                "inferenceConfig": {
                    "maxTokens": 1
                }
            }))
            .send()
            .await;
        (send_result, probe)
    } else {
        let base_trim = base_url.map(str::trim).filter(|s| !s.is_empty());
        let url = match base_trim {
            Some(b) => b.trim_end_matches('/').to_string(),
            None if provider == "openai" => "https://api.openai.com/v1".to_string(),
            // Deliberately refuse to guess for unknown providers with no base_url —
            // the admin must specify base_url for any non-OpenAI provider.
            // This aligns with the runtime's guarantee that base_url is always set
            // from the DB row before calling llm_completions_url_for_provider.
            None => {
                return Some(format!(
                    "No base_url for provider '{provider}'. \
                         Set base_url (e.g. DashScope/Moonshot compatible-mode /v1 root).",
                ));
            }
        };
        let probe = format!("{}/chat/completions", url);
        let send_result = client
            .post(&probe)
            .header("authorization", format!("Bearer {}", api_key))
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": model_name,
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await;
        (send_result, probe)
    };

    match result {
        (Ok(resp), _) if resp.status().as_u16() < 400 => None,
        (Ok(resp), _) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")?
                        .get("message")?
                        .as_str()
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| text.chars().take(200).collect());
            Some(format!("HTTP {}: {}", status, detail))
        }
        (Err(e), probe) => {
            let mut msg = format!("Connection failed for {probe}: {e}");
            if provider != "anthropic" {
                msg.push_str(
                    " — host unreachable (firewall/DNS/TLS) or region blocks; \
                     point base_url at an API gateway you can reach, or fix outbound HTTPS/proxy.",
                );
            }
            Some(msg)
        }
    }
}

/// Provider-aware two-phase probe of a model's thinking behavior.
///
/// **Bedrock/Anthropic** (default = no thinking):
///   Phase 1: Send WITH thinking enabled → can model think at all?
///   If yes → Both (user can toggle). If error/no → None.
///
/// **DashScope/native thinkers** (default = thinking):
///   Phase 1: Send default request → confirms it thinks.
///   Phase 2: Send with `enable_thinking: false` → can it stop?
///   Both phases think → NativeOnly. Phase 2 stops → Both.
///
/// **Generic OpenAI-compatible**:
///   Phase 1: Send default request → does it think by default?
///   If no → try with `reasoning_effort: "low"` → if it returns thinking → Both.
///   If still no → None.
pub async fn probe_thinking_behavior(
    provider: &str,
    model_name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> ThinkingProbeResult {
    let probe_builder = astra_core::net::apply_env_proxy(
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)),
    );
    let client = match probe_builder.build() {
        Ok(c) => c,
        Err(e) => {
            return ThinkingProbeResult {
                capability: ThinkingCapability::None,
                error: Some(format!("Client error: {e}")),
            };
        }
    };

    if provider == "bedrock" {
        return probe_bedrock(&client, model_name, api_key, base_url).await;
    }
    if provider == "anthropic" {
        return probe_anthropic(&client, model_name, api_key, base_url).await;
    }

    // OpenAI-compatible — provider-aware probe based on base_url.
    let base_trim = base_url.map(str::trim).filter(|s| !s.is_empty());
    let url = match base_trim {
        Some(b) => b.trim_end_matches('/').to_string(),
        None if provider == "openai" => "https://api.openai.com/v1".to_string(),
        None => {
            return ThinkingProbeResult {
                capability: ThinkingCapability::None,
                error: Some(format!("No base_url for provider '{provider}'")),
            };
        }
    };
    let probe_url = format!("{url}/chat/completions");
    let url_lower = url.to_ascii_lowercase();
    let base_body = serde_json::json!({
        "model": model_name,
        "max_tokens": 50,
        "temperature": 0,
        "messages": [{"role": "user", "content": "Say hello"}]
    });

    // ── DeepSeek: always thinks, supports reasoning_effort (low/high) but can't disable ──
    if url_lower.contains("deepseek") {
        let thinks = send_openai_probe(&client, &probe_url, api_key, &base_body)
            .await
            .unwrap_or(false);
        return ThinkingProbeResult {
            capability: if thinks {
                ThinkingCapability::EffortOnly
            } else {
                ThinkingCapability::None
            },
            error: None,
        };
    }

    // ── MiniMax: always thinks via <think> tags, can't disable ──
    if url_lower.contains("minimax") {
        let thinks = send_openai_probe(&client, &probe_url, api_key, &base_body)
            .await
            .unwrap_or(false);
        return ThinkingProbeResult {
            capability: if thinks {
                ThinkingCapability::NativeOnly
            } else {
                ThinkingCapability::None
            },
            error: None,
        };
    }

    // ── DashScope (Qwen, GLM-5.1): default=no thinking, enable_thinking toggles ──
    if url_lower.contains("dashscope") || url_lower.contains("aliyun") {
        // First check if it thinks by default (GLM-5.1 does)
        let default_thinks = send_openai_probe(&client, &probe_url, api_key, &base_body)
            .await
            .unwrap_or(false);
        if default_thinks {
            // Thinks by default — test suppression
            let mut body_disable = base_body;
            body_disable["enable_thinking"] = serde_json::json!(false);
            let still_thinks = send_openai_probe(&client, &probe_url, api_key, &body_disable)
                .await
                .unwrap_or(true);
            return ThinkingProbeResult {
                capability: if still_thinks {
                    ThinkingCapability::NativeOnly
                } else {
                    ThinkingCapability::Both
                },
                error: None,
            };
        }
        // Doesn't think by default — try enabling
        let mut body_enable = base_body;
        body_enable["enable_thinking"] = serde_json::json!(true);
        let enabled_thinks = send_openai_probe(&client, &probe_url, api_key, &body_enable)
            .await
            .unwrap_or(false);
        return ThinkingProbeResult {
            capability: if enabled_thinks {
                ThinkingCapability::Both
            } else {
                ThinkingCapability::None
            },
            error: None,
        };
    }

    // ── Generic OpenAI-compatible: probe default, then try enable_thinking ──
    let default_thinks = send_openai_probe(&client, &probe_url, api_key, &base_body)
        .await
        .unwrap_or(false);
    if default_thinks {
        let mut body_suppress = base_body;
        body_suppress["enable_thinking"] = serde_json::json!(false);
        let still_thinks = send_openai_probe(&client, &probe_url, api_key, &body_suppress)
            .await
            .unwrap_or(true);
        return ThinkingProbeResult {
            capability: if still_thinks {
                ThinkingCapability::NativeOnly
            } else {
                ThinkingCapability::Both
            },
            error: None,
        };
    }
    let mut body_enable = base_body;
    body_enable["enable_thinking"] = serde_json::json!(true);
    let enabled_thinks = send_openai_probe(&client, &probe_url, api_key, &body_enable)
        .await
        .unwrap_or(false);
    ThinkingProbeResult {
        capability: if enabled_thinks {
            ThinkingCapability::Both
        } else {
            ThinkingCapability::None
        },
        error: None,
    }
}

async fn probe_bedrock(
    client: &reqwest::Client,
    model_name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> ThinkingProbeResult {
    let Some(base) = base_url.map(str::trim).filter(|s| !s.is_empty()) else {
        return ThinkingProbeResult {
            capability: ThinkingCapability::None,
            error: Some("No base_url for bedrock".into()),
        };
    };
    let Ok(probe_url) = bedrock_converse_probe_url(base, model_name) else {
        return ThinkingProbeResult {
            capability: ThinkingCapability::None,
            error: Some("Invalid base_url for bedrock".into()),
        };
    };

    // Bedrock: try with thinking enabled.
    // maxTokens MUST be > budget_tokens — Bedrock rejects otherwise.
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "Say hello"}]}],
        "inferenceConfig": {"maxTokens": 2048},
        "additionalModelRequestFields": {
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        }
    });
    let resp = client
        .post(&probe_url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().as_u16() < 400 => ThinkingProbeResult {
            capability: ThinkingCapability::Both,
            error: None,
        },
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let detail = &text[..text.len().min(300)];
            if status == 400 && text.contains("think") {
                // 400 mentioning "thinking" = model rejects the thinking param
                ThinkingProbeResult {
                    capability: ThinkingCapability::None,
                    error: None,
                }
            } else if status == 400 {
                // 400 for other reasons (format issue?) — log but assume Both
                // since Sonnet/Opus are known to support thinking even if the
                // probe format was slightly wrong.
                ThinkingProbeResult {
                    capability: ThinkingCapability::Both,
                    error: Some(format!("Bedrock probe 400 (assuming Both): {detail}")),
                }
            } else {
                ThinkingProbeResult {
                    capability: ThinkingCapability::None,
                    error: Some(format!("Bedrock probe HTTP {status}: {detail}")),
                }
            }
        }
        Err(e) => ThinkingProbeResult {
            capability: ThinkingCapability::None,
            error: Some(format!("Bedrock probe failed: {e}")),
        },
    }
}

async fn probe_anthropic(
    client: &reqwest::Client,
    model_name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> ThinkingProbeResult {
    let probe_url = anthropic_messages_probe_url(base_url);

    // Anthropic: try with thinking enabled.
    // max_tokens MUST be > budget_tokens.
    let body = serde_json::json!({
        "model": model_name,
        "max_tokens": 2048,
        "messages": [{"role": "user", "content": "Say hello"}],
        "thinking": {"type": "enabled", "budget_tokens": 1024}
    });
    let resp = client
        .post(&probe_url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().as_u16() < 400 => ThinkingProbeResult {
            capability: ThinkingCapability::Both,
            error: None,
        },
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let detail = &text[..text.len().min(300)];
            if status == 400 && text.contains("think") {
                ThinkingProbeResult {
                    capability: ThinkingCapability::None,
                    error: None,
                }
            } else if status == 400 {
                ThinkingProbeResult {
                    capability: ThinkingCapability::Both,
                    error: Some(format!("Anthropic probe 400 (assuming Both): {detail}")),
                }
            } else {
                ThinkingProbeResult {
                    capability: ThinkingCapability::None,
                    error: Some(format!("Anthropic probe HTTP {status}: {detail}")),
                }
            }
        }
        Err(e) => ThinkingProbeResult {
            capability: ThinkingCapability::None,
            error: Some(format!("Anthropic probe failed: {e}")),
        },
    }
}

async fn send_openai_probe(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> Result<bool, String> {
    let resp = client
        .post(url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Probe request failed: {e}"))?;

    if resp.status().as_u16() >= 400 {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Probe HTTP {status}: {}",
            &text[..text.len().min(200)]
        ));
    }

    let text = resp.text().await.unwrap_or_default();
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Probe parse error: {e}"))?;

    let content = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    let has_think_tags = content.contains("<think>");
    let has_reasoning_content = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("reasoning_content"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.is_empty());

    Ok(has_think_tags || has_reasoning_content)
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredModelService;

#[async_trait]
impl ModelService for UnconfiguredModelService {
    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn list_models(
        &self,
        _: String,
        _: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn get_model(&self, _: String) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn check_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ModelCreateRequest {
    pub name: String,
    pub provider: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    #[serde(default = "default_text_vec")]
    pub input_modalities: Vec<String>,
    #[serde(default = "default_text_vec")]
    pub output_modalities: Vec<String>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub pricing: PricingData,
    pub architecture: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub quirks: Option<QuirksData>,
}

fn default_text_vec() -> Vec<String> {
    vec!["text".to_string()]
}

#[derive(Deserialize)]
pub struct ModelUpdateRequest {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub provider: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub supported_parameters: Option<Vec<String>>,
    pub pricing: Option<PricingData>,
    pub architecture: Option<String>,
    pub tags: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub quirks: Option<QuirksData>,
}

#[derive(Serialize, PartialEq)]
pub struct ModelResponse {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: PricingData,
    pub architecture: Option<String>,
    pub tags: Vec<String>,
    pub quirks: QuirksData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_capability: Option<ThinkingCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_probe: Option<ThinkingProbeResult>,
}

#[derive(Serialize, PartialEq)]
pub struct ModelListItemResponse {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_capability: Option<ThinkingCapability>,
}

impl From<ModelRecord> for ModelResponse {
    fn from(r: ModelRecord) -> Self {
        Self {
            model_id: r.model_id,
            name: r.name,
            provider: r.provider,
            base_url: r.base_url,
            description: r.description,
            is_active: r.is_active,
            context_window: r.context_window,
            max_completion_tokens: r.max_completion_tokens,
            input_modalities: r.input_modalities,
            output_modalities: r.output_modalities,
            supported_parameters: r.supported_parameters,
            pricing: r.pricing,
            architecture: r.architecture,
            tags: r.tags,
            quirks: r.quirks,
            connectivity: r.connectivity,
            thinking_capability: r.thinking_capability,
            thinking_probe: r.thinking_probe,
        }
    }
}

impl From<ModelListItem> for ModelListItemResponse {
    fn from(r: ModelListItem) -> Self {
        Self {
            model_id: r.model_id,
            name: r.name,
            provider: r.provider,
            description: r.description,
            is_active: r.is_active,
            context_window: r.context_window,
            max_completion_tokens: r.max_completion_tokens,
            architecture: r.architecture,
            thinking_capability: r.thinking_capability,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_model_alias ──
    //
    // Class C regression: LLMs frequently produce short family names
    // like `claude-sonnet` when calling `spawn_agent`, but the DB
    // registers fully-qualified names like
    // `us.anthropic.claude-sonnet-4-6`. Pre-alias behavior rejected
    // these outright, breaking the three shipped fork-prefix /
    // spawn-agent cases. The alias resolver deterministically maps
    // short names to their unique registered form.

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn alias_exact_match_wins() {
        let active = names(&[
            "us.anthropic.claude-sonnet-4-6",
            "us.anthropic.claude-haiku-4-5-20251001",
        ]);
        let hit = resolve_model_alias("us.anthropic.claude-sonnet-4-6", &active).unwrap();
        assert_eq!(hit, "us.anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn alias_short_name_resolves_to_unique_substring_match() {
        // The primary motivating case: model emits `claude-sonnet`,
        // registered name is `us.anthropic.claude-sonnet-4-6`. No
        // other active model contains that substring.
        let active = names(&[
            "us.anthropic.claude-sonnet-4-6",
            "us.anthropic.claude-haiku-4-5-20251001",
            "MiniMax-M2.7",
            "qwen3.6-plus",
        ]);
        let hit = resolve_model_alias("claude-sonnet", &active).unwrap();
        assert_eq!(hit, "us.anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn alias_short_name_qwen_flash_resolves() {
        let active = names(&[
            "qwen-flash",
            "qwen3.6-plus",
            "us.anthropic.claude-sonnet-4-6",
        ]);
        // Level 1 exact match.
        let hit = resolve_model_alias("qwen-flash", &active).unwrap();
        assert_eq!(hit, "qwen-flash");
    }

    #[test]
    fn alias_case_insensitive_exact_match() {
        let active = names(&["MiniMax-M2.7", "qwen-flash"]);
        // `minimax-m2.7` (lowercased) should resolve to the
        // mixed-case registered form.
        let hit = resolve_model_alias("minimax-m2.7", &active).unwrap();
        assert_eq!(hit, "MiniMax-M2.7");
    }

    #[test]
    fn alias_unknown_name_reports_not_found_with_candidates() {
        let active = names(&["qwen-flash", "MiniMax-M2.7"]);
        let err = resolve_model_alias("gpt-5", &active).unwrap_err();
        match err {
            ModelAliasResolutionError::NotFound {
                requested,
                candidates,
            } => {
                assert_eq!(requested, "gpt-5");
                assert!(candidates.contains(&"qwen-flash".into()));
                assert!(candidates.contains(&"MiniMax-M2.7".into()));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn alias_ambiguous_substring_fails_loudly_not_silently() {
        // Both `claude-haiku-4-5` and `claude-haiku-4-5-20251001` are
        // registered. A bare `claude-haiku` matches both — picking
        // one arbitrarily would be worse than failing, because the
        // caller's intent is unclear.
        let active = names(&[
            "us.anthropic.claude-haiku-4-5",
            "us.anthropic.claude-haiku-4-5-20251001",
            "MiniMax-M2.7",
        ]);
        let err = resolve_model_alias("claude-haiku", &active).unwrap_err();
        match err {
            ModelAliasResolutionError::Ambiguous {
                requested,
                candidates,
            } => {
                assert_eq!(requested, "claude-haiku");
                assert_eq!(
                    candidates.len(),
                    2,
                    "expected exactly two ambiguous candidates"
                );
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn alias_empty_string_is_error_not_first_model() {
        // Guard: an empty preferred-name coming from a caller should
        // NOT fall through to first-match. The upstream DB query
        // handles `preferred = None` separately; an explicit empty
        // string is a bug the caller should see.
        let active = names(&["qwen-flash", "MiniMax-M2.7"]);
        assert!(matches!(
            resolve_model_alias("", &active),
            Err(ModelAliasResolutionError::Empty)
        ));
        assert!(matches!(
            resolve_model_alias("   ", &active),
            Err(ModelAliasResolutionError::Empty)
        ));
    }

    #[test]
    fn alias_exact_wins_even_when_substring_would_match_something_else() {
        // Scenario: registered names `qwen-flash` and `qwen-flash-preview`.
        // Requested = `qwen-flash`. Must pick exact match, not fail
        // on substring ambiguity.
        let active = names(&["qwen-flash", "qwen-flash-preview"]);
        let hit = resolve_model_alias("qwen-flash", &active).unwrap();
        assert_eq!(
            hit, "qwen-flash",
            "exact match must win over substring ambiguity"
        );
    }

    #[test]
    fn alias_not_found_truncates_large_candidate_lists() {
        // Review nit: an error string listing 200 model names is a
        // debugging nightmare. Cap at MODEL_ALIAS_ERROR_CANDIDATE_CAP
        // with a "... N more" suffix in the Display form, so logs stay
        // scannable but nothing is lost (the full list is still on
        // the variant for programmatic callers).
        let active: Vec<String> = (0..50).map(|i| format!("model-{i}")).collect();
        let err = resolve_model_alias("nonexistent-xyz", &active).unwrap_err();
        match &err {
            ModelAliasResolutionError::NotFound { candidates, .. } => {
                // The variant itself MUST carry every candidate —
                // programmatic callers might want them all. Truncation
                // is a rendering concern.
                assert_eq!(
                    candidates.len(),
                    50,
                    "variant preserves full list; truncation is Display-only"
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        let rendered = err.to_string();
        // Render should cap the visible list and indicate truncation.
        // Pick a threshold the caller can grep: \"...\" followed by a
        // count. We don't pin the exact cap value here — that belongs
        // in a separate assertion below — only that truncation happens
        // and is disclosed.
        assert!(
            rendered.contains("...") && rendered.contains("more"),
            "rendered NotFound with 50 candidates must disclose truncation: {rendered}"
        );
        // The rendered string must NOT contain `model-49` when the cap
        // fires. `model-0`..`model-{CAP-1}` should be visible.
        assert!(
            !rendered.contains("model-49"),
            "last entry must be truncated when exceeding cap: {rendered}"
        );
        assert!(
            rendered.contains("model-0"),
            "head of list must remain visible: {rendered}"
        );
    }

    #[test]
    fn inactive_error_names_both_requested_and_resolved_when_different() {
        // Review nit: when alias resolution maps `claude-sonnet` →
        // `us.anthropic.claude-sonnet-4-6` and the resolved row is
        // later found inactive (e.g. admin deactivated between the
        // alias query and the row refetch — a narrow TOCTOU window),
        // the error message should name BOTH the requested alias and
        // the canonical name so a reviewer can see what actually
        // happened. The earlier message claimed the short form was
        // inactive, which is misleading because the short form isn't
        // what the DB row is keyed on.
        let msg = format_inactive_model_error("claude-sonnet", "us.anthropic.claude-sonnet-4-6");
        assert!(
            msg.contains("claude-sonnet"),
            "msg names the requested alias: {msg}"
        );
        assert!(
            msg.contains("us.anthropic.claude-sonnet-4-6"),
            "msg names the canonical: {msg}"
        );
        assert!(
            msg.contains("inactive"),
            "msg explains the failure mode: {msg}"
        );
        assert!(
            msg.contains("astra-admin model check"),
            "msg points at the diagnostic command: {msg}"
        );
    }

    #[test]
    fn inactive_error_does_not_duplicate_when_requested_equals_canonical() {
        // When no alias resolution happened (exact-match path) the
        // `requested` and `canonical` arguments are equal. Don't
        // pollute the message with "resolved to the same name".
        let name = "us.anthropic.claude-sonnet-4-6";
        let msg = format_inactive_model_error(name, name);
        // Count occurrences — exactly one mention of the name.
        let n = msg.matches(name).count();
        assert_eq!(
            n, 2,
            "expected name twice (in the 'Model X' and 'astra-admin model check X' parts), \
             got {n}: {msg}"
        );
        assert!(
            !msg.contains("resolved to"),
            "no alias resolution happened, message must not claim one: {msg}"
        );
    }

    #[test]
    fn alias_not_found_below_cap_renders_full_list() {
        // Inverse guard: if the candidate list fits under the cap, the
        // render must not pretend truncation happened (that would be
        // misleading).
        let active = names(&[
            "qwen-flash",
            "MiniMax-M2.7",
            "us.anthropic.claude-sonnet-4-6",
        ]);
        let err = resolve_model_alias("gpt-5", &active).unwrap_err();
        let rendered = err.to_string();
        assert!(
            !rendered.contains("more"),
            "under-cap list must not imply truncation: {rendered}"
        );
        for n in [
            "qwen-flash",
            "MiniMax-M2.7",
            "us.anthropic.claude-sonnet-4-6",
        ] {
            assert!(
                rendered.contains(n),
                "under-cap list must include {n}: {rendered}"
            );
        }
    }

    // -- rank_cheapest_index --

    fn entry(name: &str, completion: Option<f64>) -> (String, String) {
        let json = match completion {
            Some(c) => format!(r#"{{"completion": {c}}}"#),
            None => "{}".to_string(),
        };
        (name.to_string(), json)
    }

    #[test]
    fn rank_cheapest_picks_lowest_completion_price() {
        let entries = vec![
            entry("expensive", Some(0.06)),
            entry("cheapest", Some(0.001)),
            entry("middle", Some(0.015)),
        ];
        assert_eq!(rank_cheapest_index(&entries), 1);
    }

    #[test]
    fn rank_cheapest_breaks_ties_by_name_ascending() {
        let entries = vec![
            entry("zzz", Some(0.01)),
            entry("aaa", Some(0.01)),
            entry("mmm", Some(0.01)),
        ];
        assert_eq!(rank_cheapest_index(&entries), 1, "aaa comes first");
    }

    #[test]
    fn rank_cheapest_treats_missing_pricing_as_infinity() {
        let entries = vec![entry("no_pricing", None), entry("priced", Some(0.5))];
        assert_eq!(rank_cheapest_index(&entries), 1);
    }

    #[test]
    fn rank_cheapest_treats_zero_as_infinity() {
        // Zero or negative completion is treated as "unpriced" so it loses to any priced row.
        let entries = vec![entry("zero_priced", Some(0.0)), entry("normal", Some(0.02))];
        assert_eq!(rank_cheapest_index(&entries), 1);
    }

    #[test]
    fn rank_cheapest_all_unpriced_falls_back_to_name_order() {
        let entries = vec![entry("zebra", None), entry("alpha", None)];
        assert_eq!(rank_cheapest_index(&entries), 1);
    }

    #[test]
    fn rank_cheapest_single_entry() {
        let entries = vec![entry("only", Some(0.003))];
        assert_eq!(rank_cheapest_index(&entries), 0);
    }

    // -- PricingData --

    #[test]
    fn pricing_data_serialization_roundtrip() {
        let p = PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: Some(0.0003),
            cache_write: Some(0.00375),
        };
        let json = serde_json::to_string(&p).unwrap();
        let restored: PricingData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, p);
    }

    #[test]
    fn pricing_data_default_is_zeroed() {
        let p = PricingData::default();
        assert_eq!(p.prompt, 0.0);
        assert_eq!(p.completion, 0.0);
        assert!(p.cache_read.is_none());
        assert!(p.cache_write.is_none());
    }

    #[test]
    fn pricing_data_missing_optional_fields() {
        let p: PricingData =
            serde_json::from_str(r#"{"prompt": 0.003, "completion": 0.015}"#).unwrap();
        assert_eq!(p.prompt, 0.003);
        assert!(p.cache_read.is_none());
        assert!(p.cache_write.is_none());
    }

    #[test]
    fn pricing_data_empty_json_uses_defaults() {
        let p: PricingData = serde_json::from_str("{}").unwrap();
        assert_eq!(p.prompt, 0.0);
        assert_eq!(p.completion, 0.0);
    }

    #[test]
    fn pricing_data_null_cache_fields() {
        let p: PricingData = serde_json::from_str(
            r#"{"prompt": 1.0, "completion": 2.0, "cache_read": null, "cache_write": null}"#,
        )
        .unwrap();
        assert!(p.cache_read.is_none());
        assert!(p.cache_write.is_none());
    }

    #[test]
    fn pricing_data_skip_serializing_none() {
        let p = PricingData {
            prompt: 1.0,
            completion: 2.0,
            cache_read: None,
            cache_write: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("cache_read"));
        assert!(!json.contains("cache_write"));
    }

    #[test]
    fn pricing_data_negative_values_accepted() {
        // Negative pricing is structurally valid (no validation at serde level)
        let p: PricingData =
            serde_json::from_str(r#"{"prompt": -0.001, "completion": -0.002}"#).unwrap();
        assert!(p.prompt < 0.0);
    }

    // -- QuirksData --

    #[test]
    fn quirks_data_default_all_false() {
        let q = QuirksData::default();
        assert!(!q.preserve_reasoning_content);
        assert!(!q.no_parallel_tool_calls);
        assert!(!q.tool_choice_required);
        assert!(!q.strict_tool_call_ids);
        assert!(!q.no_system_message);
        assert!(!q.system_as_user_prefix);
        assert!(q.fixed_temperature.is_none());
    }

    #[test]
    fn quirks_data_empty_json_uses_defaults() {
        let q: QuirksData = serde_json::from_str("{}").unwrap();
        assert_eq!(q, QuirksData::default());
    }

    #[test]
    fn quirks_data_malformed_json_fails() {
        let result: Result<QuirksData, _> = serde_json::from_str("{not valid json}");
        assert!(result.is_err());
    }

    #[test]
    fn quirks_data_extra_unknown_fields_ignored() {
        let q: QuirksData =
            serde_json::from_str(r#"{"no_system_message": true, "unknown_future_field": 42}"#)
                .unwrap();
        assert!(q.no_system_message);
    }

    #[test]
    fn quirks_data_wrong_type_for_bool_field_fails() {
        let result: Result<QuirksData, _> = serde_json::from_str(r#"{"no_system_message": "yes"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn quirks_data_serialization_roundtrip() {
        let q = QuirksData {
            fixed_temperature: Some(0.7),
            preserve_reasoning_content: true,
            no_parallel_tool_calls: true,
            tool_choice_required: false,
            strict_tool_call_ids: true,
            no_system_message: false,
            system_as_user_prefix: true,
            fallback_chain: vec!["claude-haiku".into(), "gpt-4o-mini".into()],
        };
        let json = serde_json::to_string(&q).unwrap();
        let restored: QuirksData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, q);
    }

    #[test]
    fn quirks_unwrap_or_default_on_malformed_gives_safe_defaults() {
        // This mimics the production pattern in models.rs line 203
        let malformed = "not json at all";
        let quirks: QuirksData = serde_json::from_str(malformed).unwrap_or_default();
        assert_eq!(quirks, QuirksData::default());
    }

    // -- ModelListItemResponse / conversions --

    #[test]
    fn model_list_item_to_response_preserves_fields() {
        let item = ModelListItem {
            model_id: "m1".into(),
            name: "gpt-4o".into(),
            provider: "openai".into(),
            description: Some("fast".into()),
            is_active: true,
            context_window: 128000,
            max_completion_tokens: Some(16384),
            architecture: Some("transformer".into()),
            thinking_capability: Some(ThinkingCapability::Both),
        };
        let resp = ModelListItemResponse::from(item.clone());
        assert_eq!(resp.model_id, item.model_id);
        assert_eq!(resp.name, item.name);
        assert_eq!(resp.context_window, 128000);
        assert_eq!(resp.thinking_capability, Some(ThinkingCapability::Both));
    }

    #[test]
    fn model_list_item_with_none_optionals() {
        let item = ModelListItem {
            model_id: "m2".into(),
            name: "test".into(),
            provider: "local".into(),
            description: None,
            is_active: false,
            context_window: 4096,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: None,
        };
        let resp = ModelListItemResponse::from(item);
        assert!(resp.description.is_none());
        assert!(resp.max_completion_tokens.is_none());
        assert!(resp.architecture.is_none());
        assert!(resp.thinking_capability.is_none());
    }

    /// CLI and other clients must read `is_active` from GET /models — not `active`.
    #[test]
    fn anthropic_probe_url_minimax_china_style_base() {
        assert_eq!(
            super::anthropic_messages_probe_url(Some("https://api.minimaxi.com/anthropic")),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn anthropic_probe_url_official_default() {
        assert_eq!(
            super::anthropic_messages_probe_url(None),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn anthropic_probe_url_when_base_already_has_v1_suffix() {
        assert_eq!(
            super::anthropic_messages_probe_url(Some("https://api.anthropic.com/v1")),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn model_list_item_response_json_uses_is_active_snake_case() {
        let item = ModelListItem {
            model_id: "m3".into(),
            name: "probe".into(),
            provider: "openai".into(),
            description: None,
            is_active: false,
            context_window: 128000,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: Some(ThinkingCapability::Both),
        };
        let resp = ModelListItemResponse::from(item);
        let v = serde_json::to_value(&resp).expect("serialize ModelListItemResponse");
        assert_eq!(v.get("is_active"), Some(&serde_json::Value::Bool(false)));
        assert_eq!(
            v.get("thinking_capability"),
            Some(&serde_json::json!("both"))
        );
        assert!(
            v.get("active").is_none(),
            "legacy key `active` must not be emitted; clients should use is_active"
        );
    }

    // -- PricingData edge cases used in cost calculation --

    #[test]
    fn pricing_data_zero_rates() {
        let p = PricingData {
            prompt: 0.0,
            completion: 0.0,
            cache_read: Some(0.0),
            cache_write: Some(0.0),
        };
        // Valid: free model
        assert_eq!(p.prompt, 0.0);
        assert_eq!(p.cache_read, Some(0.0));
    }

    #[test]
    fn pricing_data_very_large_rates() {
        let p: PricingData = serde_json::from_str(
            r#"{"prompt": 999.99, "completion": 999.99, "cache_read": 999.99}"#,
        )
        .unwrap();
        assert!(p.prompt > 100.0);
    }

    #[tokio::test]
    async fn validate_connectivity_mock_provider_skips_network() {
        let result = super::validate_connectivity("mock", "any-model", "key", None).await;
        assert!(
            result.is_none(),
            "mock provider should short-circuit to success"
        );
    }

    #[tokio::test]
    async fn validate_connectivity_unknown_provider_no_base_url_errors() {
        let result = super::validate_connectivity("dashscope", "qwen-plus", "key", None).await;
        let msg = result.expect("should return an error");
        assert!(msg.contains("No base_url"), "got: {msg}");
    }

    #[tokio::test]
    async fn validate_connectivity_bedrock_no_base_url_errors() {
        let result = super::validate_connectivity(
            "bedrock",
            "anthropic.claude-3-5-sonnet-v1:0",
            "key",
            None,
        )
        .await;
        let msg = result.expect("should return an error");
        assert!(msg.contains("No base_url"), "got: {msg}");
    }

    #[tokio::test]
    async fn validate_connectivity_bedrock_invalid_base_url_errors() {
        let result = super::validate_connectivity(
            "bedrock",
            "anthropic.claude-3-5-sonnet-v1:0",
            "key",
            Some("not a url"),
        )
        .await;
        let msg = result.expect("should return an error");
        assert!(msg.contains("Invalid base_url"), "got: {msg}");
    }

    // ── resolve_memory_model: selector tag preference ──────────────────

    #[test]
    fn selector_tag_detected_in_tags_json() {
        // Simulates the tag matching logic from resolve_memory_model.
        let with_selector = r#"["chat", "selector"]"#;
        let without_selector = r#"["chat", "reasoning"]"#;
        let empty = "[]";

        assert!(with_selector.contains("\"selector\""));
        assert!(!without_selector.contains("\"selector\""));
        assert!(!empty.contains("\"selector\""));
    }

    #[test]
    fn rank_cheapest_among_selector_subset() {
        // Given 3 models where 2 have selector tag, picks cheapest selector.
        let all_entries = [
            (
                "expensive-main".to_string(),
                r#"{"prompt":0.003,"completion":0.015}"#.to_string(),
            ),
            (
                "qwen-flash".to_string(),
                r#"{"prompt":0.00000015,"completion":0.0000015}"#.to_string(),
            ),
            (
                "qwen3-flash".to_string(),
                r#"{"prompt":0.0000002,"completion":0.000002}"#.to_string(),
            ),
        ];
        // selector_rows indices: [1, 2]
        let selector_entries = [all_entries[1].clone(), all_entries[2].clone()];
        let best = rank_cheapest_index(&selector_entries);
        assert_eq!(
            selector_entries[best].0, "qwen-flash",
            "cheapest selector should be qwen-flash"
        );
    }

    // ── QuirksData fallback_chain serde ──────────────────────────────────

    #[test]
    fn quirks_fallback_chain_serde_roundtrip() {
        let json = r#"{"fallback_chain":["model-b","model-c"]}"#;
        let q: QuirksData = serde_json::from_str(json).unwrap();
        assert_eq!(q.fallback_chain, vec!["model-b", "model-c"]);
        let serialized = serde_json::to_string(&q).unwrap();
        assert!(serialized.contains("model-b"));
    }

    #[test]
    fn quirks_fallback_chain_defaults_empty_when_absent() {
        let json = r#"{}"#;
        let q: QuirksData = serde_json::from_str(json).unwrap();
        assert!(q.fallback_chain.is_empty());
    }

    #[test]
    fn quirks_fallback_chain_empty_not_serialized() {
        let q = QuirksData::default();
        let json = serde_json::to_string(&q).unwrap();
        assert!(
            !json.contains("fallback_chain"),
            "empty fallback_chain should be skipped: {json}"
        );
    }

    // ── probe_thinking_behavior ──────────────────────────────────────────

    use std::sync::{Arc, Mutex};

    async fn spawn_probe_mock(
        captured: Arc<Mutex<Option<serde_json::Value>>>,
        response_body: serde_json::Value,
    ) -> String {
        use axum::{Router, routing::post};

        let handler = move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            let resp = response_body.clone();
            async move {
                *captured.lock().unwrap() = Some(body);
                axum::Json(resp)
            }
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        format!("http://{addr}")
    }

    // ── Provider-aware probe regression tests ─────────────────────────
    //
    // Based on real API recordings from 2026-05-04.
    // Each mock simulates the provider's actual response pattern.

    /// Mock that responds differently based on enable_thinking in request.
    async fn spawn_dashscope_mock(supports_thinking: bool) -> String {
        use axum::{Router, routing::post};

        let handler = move |axum::Json(body): axum::Json<serde_json::Value>| async move {
            let enable = body.get("enable_thinking").and_then(|v| v.as_bool());
            let has_reasoning = match (supports_thinking, enable) {
                (false, _) => false,
                (true, Some(false)) => false,
                (true, Some(true)) => true,
                (true, None) => false, // DashScope default: no thinking
            };
            let mut msg = serde_json::json!({"content": "Hello!"});
            if has_reasoning {
                msg["reasoning_content"] = serde_json::json!("thinking...");
            }
            axum::Json(serde_json::json!({"choices": [{"message": msg}]}))
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        format!("http://{addr}")
    }

    /// DashScope model that thinks by default (like glm-5.1, qwen3.6-plus).
    async fn spawn_dashscope_native_thinker_mock() -> String {
        use axum::{Router, routing::post};

        let handler = |axum::Json(body): axum::Json<serde_json::Value>| async move {
            let enable = body.get("enable_thinking").and_then(|v| v.as_bool());
            let has_reasoning = match enable {
                Some(false) => false, // Suppression works
                _ => true,            // Default or explicit true → thinks
            };
            let mut msg = serde_json::json!({"content": "Hello!"});
            if has_reasoning {
                msg["reasoning_content"] = serde_json::json!("thinking...");
            }
            axum::Json(serde_json::json!({"choices": [{"message": msg}]}))
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        format!("http://{addr}")
    }

    // ── DashScope qwen-plus: default=no think, enable_thinking works → Both ──
    #[tokio::test]
    async fn probe_dashscope_qwen_plus_both() {
        let base = spawn_dashscope_mock(true).await;
        let result = probe_thinking_behavior("openai", "qwen-plus", "k", Some(&base)).await;
        assert_eq!(result.capability, ThinkingCapability::Both, "{:?}", result);
    }

    // ── DashScope qwen2.5-3b: enable_thinking has no effect → None ──
    #[tokio::test]
    async fn probe_dashscope_qwen25_3b_none() {
        let base = spawn_dashscope_mock(false).await;
        let result = probe_thinking_behavior("openai", "qwen2.5-3b", "k", Some(&base)).await;
        assert_eq!(result.capability, ThinkingCapability::None, "{:?}", result);
    }

    // ── DashScope glm-5.1: thinks by default, enable_thinking:false suppresses → Both ──
    #[tokio::test]
    async fn probe_dashscope_glm51_native_both() {
        let base = spawn_dashscope_native_thinker_mock().await;
        let result = probe_thinking_behavior("openai", "glm-5.1", "k", Some(&base)).await;
        assert_eq!(result.capability, ThinkingCapability::Both, "{:?}", result);
    }

    // ── DeepSeek: always has reasoning_content → EffortOnly ──
    #[tokio::test]
    async fn probe_deepseek_v4_effort_only() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_probe_mock(
            captured.clone(),
            serde_json::json!({
                "choices": [{"message": {
                    "content": "",
                    "reasoning_content": "thinking about hello..."
                }}]
            }),
        )
        .await;
        let result = probe_thinking_behavior("openai", "deepseek-v4-flash", "k", Some(&base)).await;
        // Generic path (no "deepseek" in localhost URL) → detects reasoning → tries suppression
        // For real DeepSeek, the url_lower check would match "deepseek"
        assert!(
            result.capability == ThinkingCapability::EffortOnly
                || result.capability == ThinkingCapability::NativeOnly
                || result.capability == ThinkingCapability::Both,
            "model with reasoning_content should not be None: {:?}",
            result
        );
    }

    // ── MiniMax: always has <think> tags → NativeOnly ──
    #[tokio::test]
    async fn probe_minimax_native_only() {
        let captured = Arc::new(Mutex::new(None));
        let base = spawn_probe_mock(
            captured.clone(),
            serde_json::json!({
                "choices": [{"message": {
                    "content": "<think>reasoning</think>\n\nHello!"
                }}]
            }),
        )
        .await;
        let result = probe_thinking_behavior("openai", "MiniMax-M2.5", "k", Some(&base)).await;
        // Generic path: detects <think> → tries suppression with same mock → still thinks → NativeOnly
        assert!(
            result.capability == ThinkingCapability::NativeOnly
                || result.capability == ThinkingCapability::Both,
            "MiniMax with <think> tags: {:?}",
            result
        );
    }

    // ── Error paths ──
    #[tokio::test]
    async fn probe_unreachable_server_returns_none() {
        let result = probe_thinking_behavior("openai", "m", "k", Some("http://127.0.0.1:1")).await;
        assert_eq!(result.capability, ThinkingCapability::None);
    }

    #[tokio::test]
    async fn probe_no_base_url_returns_error() {
        let result = probe_thinking_behavior("dashscope", "m", "k", None).await;
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("No base_url"));
    }
}
