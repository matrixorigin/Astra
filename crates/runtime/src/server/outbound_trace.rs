//! Replace outbound W3C headers from the current span.
//!
//! Header overrides and saved forward headers are applied first. This runs
//! after them, so a copied inbound `traceparent` cannot win. Export off, or
//! no valid span, leaves both headers absent.

pub(crate) fn rewrite(request: &mut reqwest::Request) {
    let headers = request.headers_mut();
    headers.remove("traceparent");
    headers.remove("tracestate");
    let carrier = astra_logging::trace_context::current();
    if let Some(value) = carrier
        .traceparent
        .and_then(|value| reqwest::header::HeaderValue::from_str(&value).ok())
    {
        headers.insert("traceparent", value);
    }
    if let Some(value) = carrier
        .tracestate
        .and_then(|value| reqwest::header::HeaderValue::from_str(&value).ok())
    {
        headers.insert("tracestate", value);
    }
}
