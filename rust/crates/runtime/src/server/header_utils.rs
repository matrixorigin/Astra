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
        })
        .collect()
}

fn is_never_collected_forward_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-connection"
            | "host"
            | "content-length"
            | "content-type"
            | "cookie"
            | "set-cookie"
            | "forwarded"
            | "origin"
            | "referer"
            | "x-csrf-token"
            | "x-xsrf-token"
            | "csrf-token"
            | "x-csrftoken"
    )
}

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
            if is_never_collected_forward_header(&name) || connection_tokens.contains(&name) {
                continue;
            }

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
    fn collect_forward_headers_keeps_first_duplicate_value() {
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
    fn collect_forward_headers_filters_unforwardable_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer trusted"),
        );
        headers.insert(
            HeaderName::from_static("x-workspace-id"),
            HeaderValue::from_static("ws-123"),
        );
        headers.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("session=secret"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("text/plain"),
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
        assert!(!collected.contains_key("content-type"));
        assert!(!collected.contains_key("connection"));
        assert!(!collected.contains_key("x-hop"));
        assert_eq!(
            collected
                .get(CONNECTION_HEADER_TOKENS_KEY)
                .map(String::as_str),
            Some("upgrade,x-hop")
        );
    }
}
