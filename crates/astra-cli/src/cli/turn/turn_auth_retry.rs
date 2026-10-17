//! Authentication retry handling for a failed turn.

use crate::cli::session::session_runtime;

pub(crate) fn should_retry_after_auth_refresh(failure: &crate::TurnFailure) -> bool {
    // A catalog preflight rejection carries no admitted run or tool effects.
    // An authentication-looking error after admission must never replay input.
    let partial = &failure.partial;
    partial.run_id.is_none()
        && !partial.remote_cancel_required
        && partial.remote_cancel_run_id.is_none()
        && !partial.callback_client_detached
        && partial.tool_calls_count == 0
        && partial.tool_call_records.is_empty()
        && partial.tools_used.is_empty()
        && partial.tool_outcomes.is_none()
        && partial.interruption.is_none()
        && partial.run_transcript_messages.is_empty()
        && partial.partial_text.is_empty()
        && failure.partial.error_code.as_deref() == Some(astra_core::ErrorKind::Auth.as_str())
        && failure
            .partial
            .error_metadata
            .as_ref()
            .is_some_and(|metadata| {
                metadata.get("source").and_then(serde_json::Value::as_str) == Some("model_access")
                    && metadata
                        .get("http_status")
                        .and_then(serde_json::Value::as_u64)
                        == Some(401)
            })
}

pub(crate) async fn prepare_auth_refresh_retry(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    failure: &crate::TurnFailure,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> Option<String> {
    if !should_retry_after_auth_refresh(failure) {
        return None;
    }

    ui.show_warning("  Token expired, attempting refresh…");
    if !session_runtime::attempt_token_refresh(api, profile).await {
        return None;
    }

    let new_token = session_runtime::current_access_token(profile)?;
    ui.show_info(&format!(
        "  {} Token refreshed, retrying…",
        crate::cli::theme::icon_ok()
    ));
    Some(new_token)
}

#[cfg(test)]
mod tests {
    use super::{prepare_auth_refresh_retry, should_retry_after_auth_refresh};

    #[test]
    fn should_retry_after_auth_refresh_matches_session_auth_only() {
        for (error, expected) in [
            (
                "API Error (401): Could not validate credentials\n  Hint: Session expired — try /login",
                false,
            ),
            ("LLM provider authentication failed", false),
            ("[auth] LLM provider authentication failed", false),
            ("[auth] Model provider rejected credentials", false),
            ("rate limited", false),
        ] {
            let failure = crate::TurnFailure {
                error: error.into(),
                partial: Default::default(),
            };
            assert_eq!(
                should_retry_after_auth_refresh(&failure),
                expected,
                "{error}"
            );
        }
    }

    #[test]
    fn preflight_auth_retry_never_replays_an_admitted_or_uncertain_turn() {
        let preflight = || crate::TurnFailure {
            error: "translated preflight failure".into(),
            partial: crate::PartialTurnData {
                error_code: Some(astra_core::ErrorKind::Auth.as_str().into()),
                error_metadata: Some(
                    serde_json::json!({"source":"model_access", "http_status":401}),
                ),
                ..Default::default()
            },
        };
        assert!(should_retry_after_auth_refresh(&preflight()));
        for change in [
            |p: &mut crate::PartialTurnData| p.run_id = Some("accepted-run".into()),
            |p: &mut crate::PartialTurnData| p.remote_cancel_required = true,
            |p: &mut crate::PartialTurnData| p.remote_cancel_run_id = Some("child".into()),
            |p: &mut crate::PartialTurnData| p.tool_calls_count = 1,
            |p: &mut crate::PartialTurnData| p.tools_used.push("bash".into()),
            |p: &mut crate::PartialTurnData| p.callback_client_detached = true,
            |p: &mut crate::PartialTurnData| {
                p.interruption = Some(serde_json::json!({"kind":"stream_transport"}))
            },
            |p: &mut crate::PartialTurnData| p.partial_text = "partial response".into(),
        ] {
            let mut failure = preflight();
            change(&mut failure.partial);
            assert!(!should_retry_after_auth_refresh(&failure));
        }
    }

    #[tokio::test]
    async fn prepare_auth_refresh_retry_returns_none_for_non_auth_failure() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let failure = crate::TurnFailure {
            error: "rate limited".into(),
            partial: crate::PartialTurnData::default(),
        };
        let mut ui = crate::tests::TestUi::default();

        let token = prepare_auth_refresh_retry(&api, None, &failure, &mut ui).await;

        assert!(token.is_none());
        assert!(ui.warnings.is_empty());
        assert!(ui.infos.is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn catalog_401_refreshes_session_but_403_does_not() {
        use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
        use crate::cli::session::session_runtime::{
            ServerDefaultModel, resolve_server_default_model,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _credentials = crate::tests::isolate_credentials();
        for status in [401, 403] {
            let mut creds = CredentialsFile::default();
            creds.profiles.insert(
                "default".into(),
                Profile {
                    account_id: Some("user-id-1".into()),
                    access_token: Some("old-access".into()),
                    refresh_token: Some("old-refresh".into()),
                    ..Default::default()
                },
            );
            save_credentials(&creds).unwrap();
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/model-access"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&mock)
                .await;
            Mock::given(method("POST")).and(path("/auth/refresh"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user_id": "user-id-1", "access_token": "new-access", "refresh_token": "new-refresh"
                })))
                .expect(if status == 401 { 1 } else { 0 }).mount(&mock).await;
            let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();
            let ServerDefaultModel::Unavailable(error) =
                resolve_server_default_model(&api, "old-access").await
            else {
                panic!("catalog should reject the request");
            };
            let mut failure = super::session_runtime::model_catalog_turn_failure(error, None);
            // Recovery follows source-authored metadata, not the display wording.
            failure.error = "translated authentication message".into();
            let mut ui = crate::tests::TestUi::default();
            let token = prepare_auth_refresh_retry(&api, None, &failure, &mut ui).await;
            assert_eq!(
                token.as_deref(),
                if status == 401 {
                    Some("new-access")
                } else {
                    None
                }
            );
            assert_eq!(ui.warnings.is_empty(), status != 401);
        }
    }
}
