//! Opt-in end-to-end browser owner check: real Next RSC + two runtime apps/pools.
//! Build Web first (`cd web && npm run build`), then run this ignored test with
//! ASTRA_TEST_DB_IT=1 and an explicitly isolated ASTRA_TEST_DATABASE.
use astra_core::{JwtSettings, MatrixOneSettings, SharedPool};
use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use astra_services::{
    AuthRegisterRequestData, AuthService, DatabaseAuthService, FernetTokenEncryptor,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use serde_json::json;
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

#[path = "../../services/tests/common/browser_session.rs"]
mod fixture;
#[path = "../../services/tests/common/isolated_database.rs"]
mod isolated_database;

struct Healthy;
#[async_trait]
impl HealthChecker for Healthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne, pinned Node, Chromium and a fresh Next production build"]
async fn real_next_render_across_two_web_and_runtime_instances() {
    let db = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&db.database);
    astra_services::storage::ensure_core_schema(&db, "mysql")
        .await
        .unwrap();
    let pool = SharedPool::new(&db).await.unwrap();
    let jwt = JwtSettings {
        secret_key: fixture::JWT_KEY.into(),
        algorithm: "HS256".into(),
        access_token_expire_minutes: 5,
        refresh_token_expire_days: 1,
    };
    let make_auth = |pool| {
        Arc::new(
            DatabaseAuthService::new(db.clone(), jwt.clone())
                .with_pool(pool)
                .with_encryptor(FernetTokenEncryptor::new(fixture::ENCRYPTION_KEY).unwrap()),
        )
    };
    let auth = make_auth(pool.clone());
    let replica = make_auth(SharedPool::new(&db).await.unwrap());
    let apps = [auth.clone(), replica].map(|auth| {
        build_app(
            AppState::new(ServiceInfo::default(), Arc::new(Healthy))
                .with_shared_pool(pool.clone())
                .with_auth_service(auth),
        )
    });
    let other_app = apps[1].clone();
    let failure_app = apps[0].clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    // Round-robin ingress keeps one public runtime binding, just as a deployment
    // load balancer does; the two auth services have independent caches/pools.
    let ingress = Router::new().fallback(move |request: Request<Body>| {
        let app = apps[counter.fetch_add(1, Ordering::Relaxed) % 2].clone();
        async move { app.oneshot(request).await.unwrap() }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runtime_url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, ingress).await.unwrap();
    });
    let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_runtime_url = format!("http://{}/", other_listener.local_addr().unwrap());
    let other_server = tokio::spawn(async move {
        axum::serve(other_listener, other_app).await.unwrap();
    });
    // Keep real login/database behavior while failing only the subsequent
    // browser-owner adoption at a different runtime address.
    let failing_runtime = Router::new()
        .route(
            "/auth/browser-sessions",
            post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .fallback(move |request: Request<Body>| {
            let app = failure_app.clone();
            async move { app.oneshot(request).await.unwrap() }
        });
    let failure_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failure_url = format!("http://{}/", failure_listener.local_addr().unwrap());
    let failure_server = tokio::spawn(async move {
        axum::serve(failure_listener, failing_runtime)
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let name = format!("next_{}", uuid::Uuid::new_v4().simple());
    let user = auth
        .register(AuthRegisterRequestData {
            username: name.clone(),
            email: format!("{name}@example.test"),
            password: "next-browser-test".into(),
            display_name: None,
        })
        .await
        .unwrap();
    // Even a deployment configured with a REAL valid account bearer must not
    // grant its identity to an anonymous or caller-selected default browser.
    let deployment_tokens = auth
        .login(astra_services::AuthLoginRequestData {
            username: name.clone(),
            password: "next-browser-test".into(),
        })
        .await
        .unwrap();
    let web = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web");
    assert!(
        web.join(".next/BUILD_ID").is_file(),
        "build the current Web source first"
    );
    let logs = tempfile::tempdir().unwrap();
    let mut processes = Vec::new();
    let mut urls = Vec::new();
    for index in 0..2 {
        let port_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = port_probe.local_addr().unwrap().port();
        drop(port_probe);
        let log = std::fs::File::create(logs.path().join(format!("next-{index}.log"))).unwrap();
        let child = tokio::process::Command::new("node")
            .current_dir(&web)
            .args([
                "node_modules/next/dist/bin/next",
                "start",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .env("ASTRA_ENABLE_E2E_PAGES", "1")
            .env("ASTRA_API_URL", &runtime_url)
            .env("ASTRA_ACCESS_TOKEN", &deployment_tokens.access_token)
            .env_remove("ASTRA_WEB_DEMO")
            .env("NO_PROXY", "localhost,127.0.0.1")
            .env("no_proxy", "localhost,127.0.0.1")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        processes.push(child);
        let url = format!("http://127.0.0.1:{port}");
        let mut ready = false;
        for _ in 0..60 {
            if client
                .get(format!("{url}/e2e/browser-session"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(
            ready,
            "fresh Next instance did not start: {}",
            std::fs::read_to_string(logs.path().join(format!("next-{index}.log"))).unwrap()
        );
        urls.push(url);
    }
    // A caller retains its independent proof across Web replicas and unknown
    // publication outcomes; the consumed pair alone cannot select the owner.
    let import_proof = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([9; 32])
    };
    let import = json!({"apiUrl":runtime_url,"accessToken":deployment_tokens.access_token,
        "refreshToken":deployment_tokens.refresh_token,"browserProof":import_proof});
    let enrolled = client
        .post(format!("{}/api/runtime-config", urls[0]))
        .json(&import)
        .send()
        .await
        .unwrap();
    assert_eq!(enrolled.status(), 200);
    let imported_cookie = enrolled
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("astra_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let recovered = client
        .post(format!("{}/api/runtime-config", urls[1]))
        .json(&import)
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), 200);
    assert!(
        recovered
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.starts_with(&format!("{imported_cookie};")))
    );
    let me = client
        .get(format!("{}/api/runtime-auth/me", urls[1]))
        .header("cookie", &imported_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 200);
    let mut wrong = import.clone();
    wrong["browserProof"] = serde_json::Value::String({
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([8; 32])
    });
    let denied = client
        .post(format!("{}/api/runtime-config", urls[1]))
        .json(&wrong)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    assert!(!denied.headers().contains_key("set-cookie"));
    let default_cookie = format!("astra_session=cfg1.{}", {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"mode":"default"}"#)
    });
    for selector in [None, Some(default_cookie.as_str())] {
        for path in ["/api/runtime-auth/me", "/api/chats", "/api/projects"] {
            let mut request = client.get(format!("{}{path}", urls[0]));
            if let Some(cookie) = selector {
                request = request.header("cookie", cookie);
            }
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status(),
                401,
                "anonymous/default selector must not inherit a valid deployment bearer: {path}"
            );
        }
        let mut request = client.get(format!("{}/api/runtime-config", urls[0]));
        if let Some(cookie) = selector {
            request = request.header("cookie", cookie);
        }
        let config: serde_json::Value = request.send().await.unwrap().json().await.unwrap();
        assert_eq!(config["hasAccessToken"], false);
    }
    let login = client
        .post(format!("{}/api/runtime-auth/login", urls[0]))
        .json(&json!({"apiUrl":runtime_url,"username":name,"password":"next-browser-test"}))
        .send()
        .await
        .unwrap();
    assert!(
        login.status().is_success(),
        "login: {}",
        login.text().await.unwrap()
    );
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("astra_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let handle = cookie.strip_prefix("astra_session=").unwrap();
    // Even if the response containing a new selector never reaches the browser,
    // the original selection remains usable on either Web replica.
    for candidate in [&runtime_url, &other_runtime_url] {
        let switched = client
            .post(format!("{}/api/runtime-auth/login", urls[0]))
            .header("cookie", &cookie)
            .json(&json!({"apiUrl":candidate,"username":name,"password":"next-browser-test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(switched.status(), 200);
        let switched_cookie = switched
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("astra_session="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        for url in &urls {
            let old = client
                .get(format!("{url}/api/runtime-auth/me"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(
                old.status(),
                200,
                "old selection survives an unreceived login response"
            );
            assert_eq!(
                client
                    .get(format!("{url}/api/runtime-auth/me"))
                    .header("cookie", &switched_cookie)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        assert_eq!(
            client
                .post(format!("{}/api/runtime-auth/logout", urls[1]))
                .header("cookie", &switched_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(format!("{}/api/runtime-auth/me", urls[0]))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "signing out the new session does not revoke the old one"
        );
    }
    let failed = client
        .post(format!("{}/api/runtime-auth/login", urls[0]))
        .header("cookie", &cookie)
        .json(&json!({"apiUrl":failure_url,"username":name,"password":"next-browser-test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 503);
    assert_eq!(
        client
            .get(format!("{}/api/runtime-auth/me", urls[1]))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "a failed cross-runtime adoption must leave the original session usable"
    );
    let old_jwt = fixture::expire_access(&pool, handle).await;
    let mut requests = tokio::task::JoinSet::new();
    let begin = tokio::time::Instant::now();
    for index in 0..12 {
        let client = client.clone();
        let cookie = cookie.clone();
        let web_url = urls[index % 2].clone();
        let user_id = user.user_id.clone();
        requests.spawn(async move {
            let response = client
                .get(format!("{web_url}/e2e/browser-session"))
                .header("cookie", cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert!(
                !response
                    .headers()
                    .get_all("set-cookie")
                    .iter()
                    .any(|v| v.to_str().unwrap().contains("astra_session"))
            );
            let body = response.text().await.unwrap();
            assert!(
                body.contains(&user_id),
                "real RSC must render the authenticated user"
            );
            assert!(!body.contains("Cookies can only be modified"));
        });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap();
    }
    eprintln!(
        "12 real Next RSC requests, two Web/two runtime owners: {:?}; ingress requests={}",
        begin.elapsed(),
        calls.load(Ordering::Relaxed)
    );
    let replay = client
        .post(format!("{runtime_url}auth/refresh"))
        .json(&json!({"refresh_token":old_jwt}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 401);
    for path in ["/api/runtime-auth/me", "/api/runtime-config"] {
        let response = client
            .get(format!("{}{path}", urls[1]))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(!response.headers().contains_key("set-cookie"));
    }
    // Exercise the actual client-rendered error boundary. HTTP-only assertions
    // cannot see that fallback or prove retry replaces the failed RSC document.
    for (path, mode) in [
        ("/?retry=preserved", "retry"),
        ("/now?retry=preserved", "retry"),
        ("/now", "sign-in"),
        ("/now", "offline-explicit"),
        ("/now", "offline-default"),
    ] {
        let login = client
            .post(format!("{}/api/runtime-auth/login", urls[0]))
            .json(&json!({"apiUrl":runtime_url,"username":name,"password":"next-browser-test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), 200);
        let broken_cookie = login
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("astra_session="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let proof = broken_cookie.strip_prefix("astra_session=").unwrap();
        sqlx::query("UPDATE auth_refresh_tokens SET browser_ciphertext = 'corrupt-test-owner' WHERE browser_handle_hash = ?")
            .bind(fixture::digest(proof)).execute(pool.get()).await.unwrap();
        let mut browser = tokio::process::Command::new("node")
            .arg(web.join("e2e/browser-session-recovery.mjs"))
            .current_dir(&web)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = json!({
            "urls":urls, "path":path, "mode":mode, "cookie":broken_cookie,
            "api_url":runtime_url, "username":name, "password":"next-browser-test",
            "user_id":user.user_id,
        })
        .to_string();
        browser
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
        let output = browser.wait_with_output().await.unwrap();
        assert!(
            output.status.success(),
            "browser recovery {path} {mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let logout = client
        .post(format!("{}/api/runtime-auth/logout", urls[1]))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 200);
    assert!(!logout.headers().contains_key("set-cookie"));
    let other_name = format!("next_other_{}", uuid::Uuid::new_v4().simple());
    let other_user = auth
        .register(AuthRegisterRequestData {
            username: other_name.clone(),
            email: format!("{other_name}@example.test"),
            password: "next-browser-test".into(),
            display_name: None,
        })
        .await
        .unwrap();
    let other_tokens = auth
        .login(astra_services::AuthLoginRequestData {
            username: other_name,
            password: "next-browser-test".into(),
        })
        .await
        .unwrap();
    let takeover = client.post(format!("{runtime_url}auth/browser-sessions"))
        .json(&json!({"handle":handle,"access_token":other_tokens.access_token,"refresh_token":other_tokens.refresh_token}))
        .send().await.unwrap();
    assert_eq!(
        takeover.status(),
        401,
        "another account cannot claim a logged-out browser handle"
    );
    assert_eq!(
        client
            .post(format!("{runtime_url}auth/refresh"))
            .json(&json!({"refresh_token":other_tokens.refresh_token}))
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "rejected adoption must not consume the other session's refresh token"
    );
    let after = client
        .get(format!("{}/api/runtime-auth/me", urls[0]))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401);
    let no_redirect = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let protected = no_redirect
        .get(format!("{}/projects", urls[0]))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(protected.status().is_redirection());
    let location = reqwest::Url::parse(&urls[0])
        .unwrap()
        .join(protected.headers()["location"].to_str().unwrap())
        .unwrap();
    assert_eq!(location.path(), "/login");
    assert_eq!(
        location
            .query_pairs()
            .find(|(key, _)| key == "next")
            .unwrap()
            .1,
        "/projects"
    );
    let public = client
        .get(format!("{}/", urls[0]))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(public.status(), 200);
    for child in &mut processes {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
    server.abort();
    other_server.abort();
    failure_server.abort();
    for table in ["auth_refresh_tokens", "auth_user_roles", "auth_users"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    }
    for table in ["auth_refresh_tokens", "auth_user_roles", "auth_users"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&other_user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    }
}
