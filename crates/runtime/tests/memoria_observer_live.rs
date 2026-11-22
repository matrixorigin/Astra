//! Opt-in contract smoke against a real, loopback-only Memoria + database.
//! Never enabled by ordinary CI or inherited service credentials alone.
#![cfg(feature = "live-provider-tests")]

use astra_runtime::{
    DatabaseTurnObserverWorker, TurnObserverOutcome, TurnObserverRequest, TurnObserverWorker,
    turn::cloud::memoria_compact::HttpMemoriaPort,
};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
#[ignore = "requires explicit opt-in and a dedicated local Memoria with owner-scoped auth"]
async fn live_memoria_owner_observe() {
    assert_eq!(
        std::env::var("ASTRA_TEST_MEMORIA_OBSERVER_LIVE").as_deref(),
        Ok("1")
    );
    let base = std::env::var("ASTRA_TEST_MEMORIA_OBSERVER_URL").expect("local test URL");
    let url = reqwest::Url::parse(&base).expect("valid test URL");
    assert_eq!(
        url.scheme(),
        "http",
        "this smoke is for a dedicated local backend"
    );
    assert!(
        url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        "remote backends are forbidden"
    );
    assert!(url.username().is_empty() && url.password().is_none());
    assert!(url.query().is_none() && url.fragment().is_none() && url.path() == "/");
    let key = std::env::var("ASTRA_TEST_MEMORIA_OBSERVER_KEY").expect("local test credential");
    assert!(!key.is_empty());
    let base = base.trim_end_matches('/');
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let owner = format!("astra_obs_{unique}_a");
    let other = format!("astra_obs_{unique}_b");
    let session = format!("observer-session-{unique}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let authorization = format!("Memoria-Owner {key}");
    // Independently pin the production server's contract before the positive
    // worker path. A legacy impersonation header must not authorize a write.
    let invalid = client
        .post(format!("{base}/v1/observe"))
        .header("Authorization", &authorization)
        .header("X-Impersonate-User", &owner)
        .json(&json!({"messages": [], "session_id": session}))
        .send()
        .await
        .expect("local negative request");
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);

    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(HttpMemoriaPort::self_hosted(
        base.to_string(),
        key,
    ))));
    let result = worker.run(TurnObserverRequest {
        standalone_guidance: false,
        user_id: owner.clone(),
        session_id: session.clone(),
        messages: vec![json!({"role": "user", "content": format!(
            "Remember for future conversations: my favorite drink is jasmine tea. My synthetic test project is ObserverCanary{unique}."
        )}).as_object().unwrap().clone()],
        turn_count: 1,
        session_start: None,
    }).await.expect("production observer must be accepted by real Memoria");
    assert_eq!(result, TurnObserverOutcome::Observed);

    for (user, should_have_memory) in [(&owner, true), (&other, false)] {
        let response = client
            .get(format!("{base}/v1/memories"))
            .header("Authorization", &authorization)
            .header("X-User-Id", user)
            .query(&[("session_id", session.as_str()), ("limit", "100")])
            .send()
            .await
            .expect("local owner read");
        assert!(response.status().is_success(), "owner read must succeed");
        let body: Value = response.json().await.expect("memory list JSON");
        let items = body["items"].as_array().expect("memory list items");
        assert_eq!(
            !items.is_empty(),
            should_have_memory,
            "memory must exist only under the bound owner"
        );
        if should_have_memory {
            assert!(items.iter().all(|item| item["session_id"] == session));
            println!(
                "real Memoria observe accepted; stored {} session memories for the bound owner",
                items.len()
            );
        }
    }
    println!("legacy header rejected with 400; second owner read is empty");
}
