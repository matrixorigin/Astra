use std::collections::{BTreeSet, HashMap};

use axum::http::{HeaderMap, HeaderName, HeaderValue};

pub(crate) const CONNECTION_HEADER_TOKENS_KEY: &str = "__astra_connection_tokens";

pub(super) fn normalize_forward_header(
    name: &HeaderName,
    value: &HeaderValue,
) -> Option<(String, String)> {
    value
        .to_str()
        .ok()
        .map(|raw| (name.as_str().to_ascii_lowercase(), raw.to_string()))
}

fn normalize_connection_tokens(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter_map(|token| {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return None;
            }

            HeaderName::from_bytes(trimmed.as_bytes())
                .ok()
                .map(|name| name.as_str().to_ascii_lowercase())
                .filter(|name| name != CONNECTION_HEADER_TOKENS_KEY)
        })
        .collect()
}

/// Header allow-list predicate: only headers explicitly required for upstream
/// routing and authentication are forwarded to upstream providers.
///
/// Strategy: allow-list (default-deny) rather than denylist (default-allow).
/// Every new header forwarded upstream must be explicitly approved here.
pub(super) fn is_allowed_forward_header(name: &str) -> bool {
    // Authentication token — required by all upstream providers.
    if name == "authorization" {
        return true;
    }

    // Internal routing/metadata headers used by the gateway.
    if name.starts_with("x-mo-") {
        return true;
    }

    // Workspace and user context for multi-tenant routing.
    if name == "x-workspace-id" || name == "x-user-id" {
        return true;
    }

    false
}

/// Collect headers to forward upstream using an allow-list strategy.
///
/// Only headers that pass `is_allowed_forward_header` are collected.
/// Hop-by-hop headers declared in the `Connection` header value are also
/// removed from the set even if they would otherwise pass the allow-list.
pub(super) fn collect_forward_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut forwarded_headers = HashMap::new();
    let mut connection_tokens = BTreeSet::new();

    for (name, value) in headers.iter() {
        if let Some((name, value)) = normalize_forward_header(name, value)
            && name == "connection"
        {
            connection_tokens.extend(normalize_connection_tokens(&value));
        }
    }

    for (name, value) in headers.iter() {
        if let Some((name, value)) = normalize_forward_header(name, value) {
            if name == CONNECTION_HEADER_TOKENS_KEY {
                continue;
            }

            // Deny hop-by-hop headers declared in Connection.
            if connection_tokens.contains(&name) {
                continue;
            }

            // Default-deny: only forward explicitly allow-listed headers.
            if !is_allowed_forward_header(&name) {
                continue;
            }

            // Duplicate inbound headers are collapsed to the first value.
            forwarded_headers.entry(name).or_insert(value);
        }
    }

    if !connection_tokens.is_empty() {
        forwarded_headers.insert(
            CONNECTION_HEADER_TOKENS_KEY.to_string(),
            connection_tokens.into_iter().collect::<Vec<_>>().join(","),
        );
    }

    forwarded_headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_authorization() {
        assert!(is_allowed_forward_header("authorization"));
    }

    #[test]
    fn allows_x_mo_headers() {
        assert!(is_allowed_forward_header("x-mo-session-id"));
        assert!(is_allowed_forward_header("x-mo-user-id"));
        assert!(is_allowed_forward_header("x-mo-routing-meta-b64"));
    }

    #[test]
    fn allows_workspace_and_user_context() {
        assert!(is_allowed_forward_header("x-workspace-id"));
        assert!(is_allowed_forward_header("x-user-id"));
    }

    #[test]
    fn blocks_arbitrary_headers() {
        assert!(!is_allowed_forward_header("cookie"));
        assert!(!is_allowed_forward_header("set-cookie"));
        assert!(!is_allowed_forward_header("host"));
        assert!(!is_allowed_forward_header("x-forwarded-for"));
        assert!(!is_allowed_forward_header("x-real-ip"));
        assert!(!is_allowed_forward_header("origin"));
        assert!(!is_allowed_forward_header("referer"));
        assert!(!is_allowed_forward_header("content-type"));
        assert!(!is_allowed_forward_header("x-api-key"));
        assert!(!is_allowed_forward_header("x-auth-token"));
    }

    #[test]
    fn blocks_prefix_spoof() {
        // "x-mobile" starts with "x-mo" but not "x-mo-"
        assert!(!is_allowed_forward_header("x-mobile"));
        assert!(is_allowed_forward_header("x-mo-"));
    }

    #[test]
    fn blocks_synthetic_connection_tokens_header() {
        assert!(!is_allowed_forward_header(CONNECTION_HEADER_TOKENS_KEY));
    }

    #[test]
    fn collect_forward_headers_keep_first_duplicate_value() {
        let mut headers = HeaderMap::new();
        headers.append(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer first"),
        );
        headers.append(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer second"),
        );

        let collected = collect_forward_headers(&headers);
        assert_eq!(
            collected.get("authorization").map(String::as_str),
            Some("Bearer first")
        );
    }

    #[test]
    fn collect_forward_headers_filters_unallowlisted_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer trusted"),
        );
        headers.insert(
            HeaderName::from_static("x-workspace-id"),
            HeaderValue::from_static("ws-123"),
        );
        // These should be dropped by the allow-list.
        headers.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("session=secret"),
        );
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("sk-should-not-leak"),
        );
        headers.insert(
            HeaderName::from_static("connection"),
            HeaderValue::from_static("x-hop, Upgrade"),
        );
        headers.insert(
            HeaderName::from_static("x-hop"),
            HeaderValue::from_static("should-not-be-collected"),
        );

        let collected = collect_forward_headers(&headers);

        assert_eq!(
            collected.get("authorization").map(String::as_str),
            Some("Bearer trusted")
        );
        assert_eq!(
            collected.get("x-workspace-id").map(String::as_str),
            Some("ws-123")
        );
        assert!(!collected.contains_key("cookie"));
        assert!(!collected.contains_key("x-api-key"));
        assert!(!collected.contains_key("connection"));
        assert!(!collected.contains_key("x-hop"));
        assert_eq!(
            collected
                .get(CONNECTION_HEADER_TOKENS_KEY)
                .map(String::as_str),
            Some("upgrade,x-hop")
        );
    }

    #[test]
    fn collect_forward_headers_drops_inbound_synthetic_connection_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer trusted"),
        );
        headers.insert(
            HeaderName::from_bytes(CONNECTION_HEADER_TOKENS_KEY.as_bytes()).unwrap(),
            HeaderValue::from_static("authorization,x-workspace-id"),
        );

        let collected = collect_forward_headers(&headers);

        assert_eq!(
            collected.get("authorization").map(String::as_str),
            Some("Bearer trusted")
        );
        assert!(
            !collected.contains_key(CONNECTION_HEADER_TOKENS_KEY),
            "synthetic control header must only be generated from a real Connection header"
        );
    }

    #[test]
    fn collect_forward_headers_real_connection_tokens_override_spoofed_synthetic_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("connection"),
            HeaderValue::from_static("x-hop, __astra_connection_tokens"),
        );
        headers.insert(
            HeaderName::from_bytes(CONNECTION_HEADER_TOKENS_KEY.as_bytes()).unwrap(),
            HeaderValue::from_static("authorization"),
        );

        let collected = collect_forward_headers(&headers);

        assert_eq!(
            collected
                .get(CONNECTION_HEADER_TOKENS_KEY)
                .map(String::as_str),
            Some("x-hop")
        );
    }
}
