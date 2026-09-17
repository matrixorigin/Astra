//! One-time authorization codes delivered exclusively through the local browser.
//! There is no remote approval polling or public login-ticket retrieval path.
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(super) fn verifier() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(super) fn append_capability(url: &str, secret: &str) -> Result<String, String> {
    let mut url = url::Url::parse(url).map_err(|_| "Invalid website login URL")?;
    url.query_pairs_mut()
        .append_pair("callback_transport", "authorization_code_v1")
        .append_pair("code_challenge_method", "S256")
        .append_pair(
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes())),
        );
    Ok(url.into())
}

pub(super) fn callback_code(target: &str, expected_state: &str) -> Result<String, String> {
    // Only an origin-form request target on our fixed callback path is accepted.
    if !target.starts_with("/callback?") || target.contains('#') {
        return Err("Invalid callback".into());
    }
    let url =
        url::Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| "Invalid callback")?;
    let mut state = None;
    let mut code = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" if code.is_none() => code = Some(value.into_owned()),
            _ => return Err("Invalid callback parameters".into()),
        }
    }
    if !super::constant_time_eq(
        state.as_deref().unwrap_or_default().as_bytes(),
        expected_state.as_bytes(),
    ) {
        return Err("Invalid callback state".into());
    }
    let code = code.ok_or("Missing authorization code")?;
    if code.len() != 43 || !URL_SAFE_NO_PAD.decode(&code).is_ok_and(|v| v.len() == 32) {
        return Err("Invalid authorization code".into());
    }
    Ok(code)
}

pub(super) async fn redeem(
    website: &str,
    code: &str,
    secret: &str,
    port: u16,
    state: &str,
) -> Result<String, String> {
    let website_url = super::validate_login_website(website)?;
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15));
    // A proxy cannot route a process-local test/dev website. Match the
    // login-URL security boundary: local HTTP endpoints are direct, while
    // remote HTTPS endpoints retain the configured proxy behavior.
    let builder = if website_url
        .host_str()
        .is_some_and(super::login_website_host_is_loopback)
    {
        builder.no_proxy()
    } else {
        builder
    };
    let client = builder
        .build()
        .map_err(|_| "Could not initialize login code exchange")?;
    let mut response = client
        .post(format!(
            "{}/api/auth/astra/browser-login/redeem",
            website.trim_end_matches('/')
        ))
        .json(
            &serde_json::json!({"authorization_code":code,"code_verifier":secret,
            "redirect_uri":format!("http://127.0.0.1:{port}/callback"),"state":state}),
        )
        .send()
        .await
        .map_err(|_| "Login code exchange failed; run astra login again")?;
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            403 => "Login code is invalid, expired, or no longer authorized; run astra login again",
            409 => "Login code has already been used; run astra login again",
            429 => "Login is rate limited; wait before running astra login again",
            _ => "Login code exchange is unavailable; run astra login again",
        }
        .into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Login response was interrupted; run astra login again")?
    {
        if bytes.len() + chunk.len() > 8192 {
            return Err("Login response is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct Redeemed {
        connection_key: String,
    }
    let result: Redeemed =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid login code response")?;
    if result.connection_key.is_empty() || result.connection_key.len() > 4096 {
        return Err("Invalid login credential".into());
    }
    Ok(result.connection_key)
}

// Only fixed, application-owned copy enters this template. Never interpolate
// a callback parameter, credential, account identity, or provider error here.
fn result_response(success: bool) -> String {
    const HTML: &str = include_str!("result_page.html");
    const STYLES: &str = include_str!("result_page.css");
    let (status, title, message, next_step, icon): (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) = if success {
        (
            "200 OK",
            "You are signed in to Astra",
            "Return to your terminal to continue.",
            "You can close this tab.",
            "m8 12 3 3 5-6",
        )
    } else {
        (
            "400 Bad Request",
            "Couldn’t complete sign-in",
            "Return to your terminal and run astra login again.",
            "Use the new link to try again.",
            "m9 9 6 6 m0-6-6 6",
        )
    };
    let body = HTML
        .replace("{{status}}", if success { "success" } else { "failure" })
        .replace("{{title}}", title)
        .replace("{{message}}", message)
        .replace("{{next_step}}", next_step)
        .replace("{{icon}}", icon)
        .replace("{{styles}}", STYLES);
    // Authorize only the embedded stylesheet, not arbitrary inline CSS or JS.
    let style_hash = STANDARD.encode(Sha256::digest(STYLES.as_bytes()));
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'sha256-{style_hash}'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub(super) async fn write_result(
    stream: &mut tokio::net::TcpStream,
    success: bool,
    deadline: tokio::time::Instant,
) {
    let response = result_response(success);
    if super::write_callback_bytes(stream, response.as_bytes(), deadline)
        .await
        .is_err()
    {
        // Presentation failure must not undo an already-persisted login or
        // obscure its original error. Never log callback contents here.
        tracing::warn!("could not deliver browser login result page");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[test]
    fn result_pages_are_self_contained_accessible_and_not_interactive() {
        for success in [true, false] {
            let response = result_response(success);
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.starts_with(if success {
                "HTTP/1.1 200 OK"
            } else {
                "HTTP/1.1 400 Bad Request"
            }));
            assert!(headers.contains("Content-Type: text/html; charset=utf-8"));
            assert!(headers.contains(&format!("Content-Length: {}\r\n", body.len())));
            for header in [
                "Cache-Control: no-store",
                "Referrer-Policy: no-referrer",
                "X-Content-Type-Options: nosniff",
                "default-src 'none'",
                "form-action 'none'",
                "base-uri 'none'",
                "frame-ancestors 'none'",
            ] {
                assert!(headers.contains(header), "missing {header}");
            }
            let css = body
                .split_once("<style>")
                .unwrap()
                .1
                .split_once("</style>")
                .unwrap()
                .0;
            let hash = STANDARD.encode(Sha256::digest(css.as_bytes()));
            assert!(headers.contains(&format!("style-src 'sha256-{hash}'")));
            assert_eq!(body.matches("<style>").count(), 1);
            assert_eq!(body.matches("</style>").count(), 1);
            assert!(!body.contains("style="));
            assert!(!headers.contains("unsafe-"));
            assert!(
                !body.contains('\r'),
                "embedded assets must use LF for CSP hashing"
            );
            for forbidden in [
                "<script",
                "<button",
                "<form",
                "<a ",
                "src=",
                "href=",
                "url(",
                "@import",
                "window.close",
                "{{",
                "authorization_code",
                "code_verifier",
                "connection_key",
            ] {
                assert!(!body.contains(forbidden), "unexpected {forbidden}");
            }
            assert!(body.contains("<html lang=\"en\">"));
            assert!(body.contains("name=\"viewport\""));
            assert!(body.contains("aria-labelledby=\"result-title\""));
            assert!(body.contains("prefers-color-scheme: dark"));
            assert!(body.contains("Astra Cloud"));
            if success {
                assert!(body.contains("class=\"card success\""));
                assert!(body.contains("d=\"m8 12 3 3 5-6\""));
                assert!(!body.contains("d=\"m9 9 6 6 m0-6-6 6\""));
                assert!(body.contains("You are signed in to Astra"));
                assert!(body.contains("You can close this tab."));
                assert!(!body.contains("astra login"));
            } else {
                assert!(body.contains("class=\"card failure\""));
                assert!(body.contains("d=\"m9 9 6 6 m0-6-6 6\""));
                assert!(!body.contains("d=\"m8 12 3 3 5-6\""));
                assert!(body.contains("Couldn’t complete sign-in"));
                assert!(body.contains("run astra login again."));
                assert!(!body.contains("You are signed in"));
            }
        }
    }

    #[tokio::test]
    async fn result_writer_delivers_complete_html_over_loopback() {
        use tokio::io::AsyncReadExt;
        for success in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap());
            let server = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                write_result(
                    &mut stream,
                    success,
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await;
            };
            let read = async {
                let mut stream = client.await.unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).await.unwrap();
                assert_eq!(response, result_response(success));
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(server, read);
            })
            .await
            .unwrap();
        }
    }

    #[test]
    fn capability_is_explicit_and_never_exposes_verifier() {
        let secret = verifier();
        let url = append_capability(
            "https://example.com/connect/astra?port=1234&state=state",
            &secret,
        )
        .unwrap();
        assert!(!url.contains(&secret));
        let url = url::Url::parse(&url).unwrap();
        let fields: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(fields["code_challenge_method"], "S256");
        assert_eq!(fields["callback_transport"], "authorization_code_v1");
        assert_eq!(
            fields["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
        );
    }

    #[test]
    fn callback_requires_matching_state_and_unambiguous_code() {
        let code = verifier();
        assert_eq!(
            callback_code(&format!("/callback?state=expected&code={code}"), "expected").unwrap(),
            code
        );
        for target in [
            format!("/callback?state=wrong&code={code}"),
            format!("/callback?state=expected&state=expected&code={code}"),
            format!("/callback?state=expected&code={code}&code={code}"),
            format!("http://evil.invalid/callback?state=expected&code={code}"),
            "/callback?state=expected&code=short".into(),
            format!("/callback?state=expected&code={code}#fragment"),
        ] {
            assert!(callback_code(&target, "expected").is_err());
        }
    }

    #[tokio::test]
    async fn exchange_rejects_malformed_and_oversized_success_without_retry() {
        for body in [
            "<html>gateway fallback</html>".to_string(),
            "{}".into(),
            serde_json::json!({"connection_key":""}).to_string(),
            serde_json::json!({"connection_key":"x".repeat(4097)}).to_string(),
            serde_json::json!({"connection_key":"test-key","padding":"x".repeat(8192)}).to_string(),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/auth/astra/browser-login/redeem"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                redeem(&server.uri(), "code", "secret", 1234, "state")
                    .await
                    .is_err()
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn exchange_is_bounded_non_redirecting_and_never_replayed() {
        for status in [200, 302, 403, 409, 429, 503] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/auth/astra/browser-login/redeem"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(serde_json::json!({"connection_key":"test-key"}))
                        .insert_header("Location", format!("{}/redirect", server.uri())),
                )
                .expect(1)
                .mount(&server)
                .await;
            let result = redeem(&server.uri(), "code", "secret", 1234, "nonce").await;
            assert_eq!(result.is_ok(), status == 200);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].body_json::<serde_json::Value>().unwrap(),
                serde_json::json!({"authorization_code":"code","code_verifier":"secret","redirect_uri":"http://127.0.0.1:1234/callback","state":"nonce"})
            );
        }
    }
}
