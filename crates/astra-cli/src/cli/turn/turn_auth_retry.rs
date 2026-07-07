//! Authentication retry handling for a failed turn.

use crate::cli::auth_flow::is_auth_error;
use crate::cli::session::session_runtime;

pub(crate) fn should_retry_after_auth_refresh(error: &str) -> bool {
    is_auth_error(error)
}

pub(crate) async fn prepare_auth_refresh_retry(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    failure: &crate::TurnFailure,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> Option<String> {
    if !should_retry_after_auth_refresh(&failure.error) {
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
        assert!(should_retry_after_auth_refresh(
            "API Error (401): Could not validate credentials\n  Hint: Session expired — try /login",
        ));
        assert!(!should_retry_after_auth_refresh(
            "LLM provider authentication failed",
        ));
        assert!(!should_retry_after_auth_refresh("rate limited"));
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
}
