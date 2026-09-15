mod common;

use astra_services::{DatabaseSessionService, SessionCreateRequestData, SessionService};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn provider_session_creation_is_idempotent_across_service_instances() {
    use astra_services::resource_governor::{LimitCheck, ResourceLimitKind};
    use astra_services::{
        AuthPrincipal, AuthPrincipalOrigin, AuthProviderAuthorizedRequestContext, AuthUserRecord,
        ProviderSessionCreationIdentity,
    };
    let (pool, settings) = common::setup_pool_and_settings().await;
    let first = DatabaseSessionService::new(settings.clone()).with_pool(pool.clone());
    let second = DatabaseSessionService::new(settings).with_pool(pool);
    let user_id = format!("creation-{}", Uuid::new_v4().simple());
    let principal = AuthPrincipal {
        user: AuthUserRecord {
            user_id: user_id.clone(),
            username: user_id.clone(),
            email: "creation@example.test".to_string(),
            display_name: None,
        },
        session_id: None,
        origin: AuthPrincipalOrigin::ProviderAuthorizedRequest(
            AuthProviderAuthorizedRequestContext {
                provider_id: "moi".to_string(),
                external_subject: user_id.clone(),
                provider_scope_id: "workspace".to_string(),
                request_authorization_id: "request".to_string(),
                edge_agent_id: None,
            },
        ),
    };
    let identity =
        ProviderSessionCreationIdentity::from_principal(&principal, "lost-session-1").unwrap();
    let request = SessionCreateRequestData {
        agent_id: None,
        title: None,
        metadata: None,
    };
    let (a, b) = tokio::join!(
        first.create_provider_session(identity.clone(), request.clone(), LimitCheck::Allowed),
        second.create_provider_session(identity.clone(), request.clone(), LimitCheck::Allowed),
    );
    let a = a.expect("first replica");
    let b = b.expect("second replica");
    assert_eq!(a.session.session_id, b.session.session_id);
    assert_ne!(a.created, b.created, "exactly one call creates the session");
    let denied = LimitCheck::Denied {
        limit: ResourceLimitKind::DailySessions,
        reason: "limit reached".to_string(),
    };
    let replay = second
        .create_provider_session(identity.clone(), request.clone(), denied.clone())
        .await
        .expect("response-loss retry must remain usable at quota");
    assert!(!replay.created);
    assert_eq!(replay.session.session_id, a.session.session_id);
    let mut changed = request.clone();
    changed.title = Some("different payload".to_string());
    let conflict = second
        .create_provider_session(identity, changed, LimitCheck::Allowed)
        .await
        .expect_err("same ref cannot replace the original payload");
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    let new_identity =
        ProviderSessionCreationIdentity::from_principal(&principal, "new-session").unwrap();
    assert_eq!(
        first
            .create_provider_session(new_identity, request, denied)
            .await
            .unwrap_err()
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    first
        .delete_session(a.session.session_id, user_id)
        .await
        .expect("delete test session");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn provider_lookup_distinguishes_absence_without_exposing_foreign_sessions() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let sessions = DatabaseSessionService::new(settings).with_pool(pool);
    let owner = format!("provider-lookup-{}", Uuid::new_v4().simple());
    let foreign = format!("provider-other-{}", Uuid::new_v4().simple());
    let session = sessions
        .create_session(
            owner.clone(),
            SessionCreateRequestData {
                agent_id: None,
                title: Some("Provider lookup contract".to_string()),
                metadata: None,
            },
        )
        .await
        .expect("create owned session");

    let found = sessions
        .get_session_for_provider_request(session.session_id.clone(), owner.clone())
        .await
        .expect("owner can continue the existing session");
    assert_eq!(found.session_id, session.session_id);
    let hidden = sessions
        .get_session_for_provider_request(session.session_id.clone(), foreign)
        .await
        .expect_err("foreign owner must not read the session");
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden.1.0.error_code, None);

    let absent_id = Uuid::new_v4().to_string();
    let absent = sessions
        .get_session_for_provider_request(absent_id.clone(), owner.clone())
        .await
        .expect_err("confirm physical absence");
    assert_eq!(absent.0, StatusCode::NOT_FOUND);
    assert_eq!(absent.1.0.error_code.as_deref(), Some("session_not_found"));
    let ordinary = sessions
        .get_session(absent_id, owner.clone())
        .await
        .expect_err("ordinary lookup retains hidden-not-found contract");
    assert_eq!(ordinary.0, StatusCode::NOT_FOUND);
    assert_eq!(ordinary.1.0.error_code, None);

    sessions
        .delete_session(session.session_id, owner)
        .await
        .expect("delete test session");
}
