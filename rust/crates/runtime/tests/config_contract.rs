use std::{collections::HashMap, fs, path::PathBuf};

use astra_runtime::config::AppSettings;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct SettingsContract {
    defaults: FlatSettings,
    override_env: HashMap<String, String>,
    expected_override_settings: FlatSettings,
    invalid_embedding_inference: InvalidEmbeddingInference,
}

#[derive(Debug, Deserialize)]
struct InvalidEmbeddingInference {
    embedding_model: String,
    embedding_dim: u32,
    error_substring: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct FlatSettings {
    matrixone_host: String,
    matrixone_port: u16,
    matrixone_user: String,
    matrixone_password: String,
    matrixone_database: String,
    redis_host: String,
    redis_port: u16,
    redis_password: Option<String>,
    app_env: String,
    log_level: String,
    secret_key: String,
    embedding_provider: String,
    embedding_model: String,
    embedding_dim: u32,
    embedding_api_key: String,
    embedding_base_url: Option<String>,
    github_token: Option<String>,
    chat_turn_bridge_url: Option<String>,
    chat_turn_bridge_secret: String,
}

fn load_contract() -> SettingsContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/settings_contract.json");
    let content = fs::read_to_string(path).expect("settings contract fixture should exist");
    serde_json::from_str(&content).expect("settings contract fixture should be valid JSON")
}

fn flatten(settings: AppSettings) -> FlatSettings {
    FlatSettings {
        matrixone_host: settings.matrixone.host,
        matrixone_port: settings.matrixone.port,
        matrixone_user: settings.matrixone.user,
        matrixone_password: settings.matrixone.password,
        matrixone_database: settings.matrixone.database,
        redis_host: settings.redis.host,
        redis_port: settings.redis.port,
        redis_password: settings.redis.password,
        app_env: settings.application.app_env,
        log_level: settings.application.log_level,
        secret_key: settings.application.secret_key,
        embedding_provider: settings.embedding.provider,
        embedding_model: settings.embedding.model,
        embedding_dim: settings.embedding.dim,
        embedding_api_key: settings.embedding.api_key,
        embedding_base_url: settings.embedding.base_url,
        github_token: settings.github_token,
        chat_turn_bridge_url: settings.chat_turn_bridge_url,
        chat_turn_bridge_secret: settings.chat_turn_bridge_secret,
    }
}

#[test]
fn defaults_match_shared_contract() {
    let contract = load_contract();
    let settings = AppSettings::from_map(&HashMap::new()).expect("defaults should parse");

    assert_eq!(flatten(settings), contract.defaults);
}

#[test]
fn overrides_match_shared_contract() {
    let contract = load_contract();
    let settings = AppSettings::from_map(&contract.override_env).expect("overrides should parse");

    assert_eq!(flatten(settings), contract.expected_override_settings);
}

#[test]
fn unknown_embedding_model_matches_shared_error_contract() {
    let contract = load_contract();
    let mut values = HashMap::new();
    values.insert(
        "EMBEDDING_MODEL".to_string(),
        contract.invalid_embedding_inference.embedding_model,
    );
    values.insert(
        "EMBEDDING_DIM".to_string(),
        contract
            .invalid_embedding_inference
            .embedding_dim
            .to_string(),
    );

    let error = AppSettings::from_map(&values).expect_err("unknown model should fail");
    assert!(
        error
            .to_string()
            .contains(&contract.invalid_embedding_inference.error_substring)
    );
}
