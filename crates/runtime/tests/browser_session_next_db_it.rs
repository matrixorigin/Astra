//! Opt-in end-to-end browser owner check: real Next RSC + two runtime apps/pools.
//! Build Web first (`cd web && npm run build`), then run this ignored test with
//! ASTRA_TEST_DB_IT=1 and an explicitly isolated ASTRA_TEST_DATABASE.
use astra_core::{JwtSettings, MatrixOneSettings, SharedPool};
use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use astra_services::{
    AuthRegisterRequestData, AuthService, DatabaseAuthService, FernetTokenEncryptor,
};
use async_trait::async_trait;
use axum::{Router, body::Body, http::Request};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
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
#[ignore = "requires isolated MatrixOne, Node >=22, npm ci and a fresh Next production build"]
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
    let logout = client
        .post(format!("{}/api/runtime-auth/logout", urls[1]))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 200);
    assert!(!logout.headers().contains_key("set-cookie"));
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
    assert!(
        protected.headers()["location"]
            .to_str()
            .unwrap()
            .ends_with("/login")
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
    for table in ["auth_refresh_tokens", "auth_user_roles", "auth_users"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    }
}
