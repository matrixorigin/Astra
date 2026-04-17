use std::collections::HashMap;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

pub(super) fn normalize_forward_header(
    name: &HeaderName,
    value: &HeaderValue,
) -> Option<(String, String)> {
    value
        .to_str()
        .ok()
        .map(|raw| (name.as_str().to_ascii_lowercase(), raw.to_string()))
}

pub(super) fn collect_forward_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| normalize_forward_header(name, value))
        .collect()
}
