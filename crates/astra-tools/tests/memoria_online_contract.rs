//! Opt-in contract test against a real Memoria service.
//!
//! This is intentionally ignored by the offline suite. Run with:
//! `ASTRA_MEMORIA_ONLINE=1 cargo test -p astra-tools --test memoria_online_contract -- --ignored`

use std::time::{SystemTime, UNIX_EPOCH};

use astra_tools::memoria::MemoriaToolGateway;
use serde_json::{Value, json};

fn parse_response(operation: &str, raw: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("{operation} returned non-JSON output: {error}; body={raw}"))?;
    if value.get("error").is_some() {
        return Err(format!("{operation} failed: {value}"));
    }
    Ok(value)
}

#[tokio::test]
#[ignore = "requires ASTRA_MEMORIA_ONLINE=1 and a real MEMORIA_BASE_URL/MASTER_KEY"]
async fn missing_identity_is_actionable_without_poisoning_live_memory_service_health() {
    assert_eq!(
        std::env::var("ASTRA_MEMORIA_ONLINE").as_deref(),
        Ok("1"),
        "explicitly opt in with ASTRA_MEMORIA_ONLINE=1"
    );
    assert!(
        std::env::var("MEMORIA_MASTER_KEY").is_ok(),
        "MEMORIA_MASTER_KEY is required for the real-service contract"
    );

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let gateway = MemoriaToolGateway::new(None, None);
    let content = format!("Astra online memory contract nonce {nonce}");
    let stored_raw = gateway
        .call(
            "remember",
            &json!({
                "content": content,
                "memory_type": "semantic",
                "skip_conflict_check": true,
            }),
        )
        .await;
    let stored = parse_response("remember", &stored_raw).unwrap();
    let memory_id = stored
        .get("memory_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .expect("real remember response must return an exact memory_id")
        .to_string();

    let outcome: Result<(), String> = async {
        for attempt in 1..=2 {
            let missing_id = format!("astra-online-missing-{nonce}-{attempt}");
            let raw = gateway
                .call(
                    "update",
                    &json!({
                        "memory_id": missing_id,
                        "content": "must not be created",
                        "reason": "verify missing-identity error contract",
                    }),
                )
                .await;
            let value: Value = serde_json::from_str(&raw)
                .map_err(|error| format!("missing update returned invalid JSON: {error}"))?;
            if value["error"]["code"] != "memory_not_found" {
                return Err(format!(
                    "missing update must return memory_not_found, got: {value}"
                ));
            }
            if gateway.is_circuit_open() {
                return Err("a deterministic 404 poisoned the availability circuit".to_string());
            }
        }

        let corrected = gateway
            .call(
                "update",
                &json!({
                    "memory_id": memory_id,
                    "content": format!("{content} corrected"),
                    "reason": "verify a valid call still succeeds after missing identities",
                }),
            )
            .await;
        parse_response("valid update after two 404 responses", &corrected)?;
        Ok(())
    }
    .await;

    let cleanup = gateway
        .call(
            "forget",
            &json!({
                "memory_id": memory_id,
                "reason": "online contract cleanup",
            }),
        )
        .await;
    let cleanup_result = parse_response("cleanup", &cleanup);

    outcome.unwrap();
    cleanup_result.unwrap();
}
