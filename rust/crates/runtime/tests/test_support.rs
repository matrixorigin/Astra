#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use astra_runtime::{
    AgenticRunLifecycleService, FernetTokenEncryptor, MatrixOneSettings, RunEngine,
};
use astra_services::InMemoryRunStateStore;
use serde_json::{Value, json};

pub type EdgeCallbackLedger = Arc<tokio::sync::Mutex<HashMap<String, Value>>>;

pub fn test_fernet_encryptor(key: &str) -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new(key).expect("fernet key"))
}

pub fn test_matrixone_settings() -> MatrixOneSettings {
    MatrixOneSettings {
        host: "127.0.0.1".into(),
        port: 0,
        user: "x".into(),
        password: "x".into(),
        database: "x".into(),
    }
}

pub fn test_run_lifecycle(
    encryptor: Arc<FernetTokenEncryptor>,
    ledger: EdgeCallbackLedger,
) -> AgenticRunLifecycleService {
    let run_engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    AgenticRunLifecycleService::new(test_matrixone_settings(), encryptor, ledger, run_engine)
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": serde_json::to_string(&args).unwrap()
        }
    })
}

pub fn tool_schema(name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": format!("{name} tool"),
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }
        }
    })
}

pub fn parse_sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}
