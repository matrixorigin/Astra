//! W3C `traceparent` / `tracestate` for Astra export.
//!
//! The process exports only after OTLP init succeeds. Until then these helpers
//! carry nothing and outbound callers strip stale trace headers.

use std::cell::RefCell;
use std::future::Future;

#[cfg(feature = "otel")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "otel")]
static EXPORT_READY: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "otel")]
pub(crate) fn mark_export_ready() {
    EXPORT_READY.store(true, Ordering::Release);
}

#[cfg(feature = "otel")]
fn export_ready() -> bool {
    EXPORT_READY.load(Ordering::Acquire)
}

/// One inbound or outbound W3C carrier. Empty fields are absent, not blank.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carrier {
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

impl Carrier {
    pub fn from_header_values(traceparent: Option<&str>, tracestate: Option<&str>) -> Self {
        Self::from_header_fields(traceparent, tracestate)
    }

    /// Combine repeated `tracestate` fields, then keep a valid W3C list.
    /// An invalid list is dropped. A valid `traceparent` is kept.
    pub fn from_header_fields<'a, I>(traceparent: Option<&str>, tracestate: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut combined = String::new();
        for field in tracestate {
            if !combined.is_empty() {
                combined.push(',');
            }
            combined.push_str(field);
        }
        Self {
            traceparent: traceparent
                .map(str::trim)
                .filter(|value| valid_traceparent(value))
                .map(str::to_owned),
            tracestate: (!combined.is_empty())
                .then(|| normalize_tracestate(&combined))
                .flatten(),
        }
    }
}

/// W3C section 3.2.4, as the SDK propagator parses it: version `00` has exactly
/// four fields; a higher version keeps the first four and ignores the rest.
pub(crate) fn valid_traceparent(value: &str) -> bool {
    let mut parts = value.splitn(5, '-');
    let (Some(version), Some(trace_id), Some(span_id), Some(flags)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let extended = parts.next().is_some();
    version.len() == 2
        && trace_id.len() == 32
        && span_id.len() == 16
        && flags.len() == 2
        && is_lower_hex(version)
        && version != "ff"
        && !(version == "00" && extended)
        && is_lower_hex(trace_id)
        && is_lower_hex(span_id)
        && is_lower_hex(flags)
        && trace_id.chars().any(|c| c != '0')
        && span_id.chars().any(|c| c != '0')
}

/// W3C `tracestate`: comma-separated members, optional whitespace around
/// commas, and spaces inside a value. The stored form drops that whitespace.
fn normalize_tracestate(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 512 {
        return None;
    }
    let mut members = Vec::new();
    for raw in value.split(',') {
        let member = trim_ows(raw);
        if member.is_empty() {
            continue;
        }
        let (key, raw_value) = member.split_once('=')?;
        if !valid_tracestate_key(key) || !valid_tracestate_value(raw_value) {
            return None;
        }
        members.push(format!("{key}={raw_value}"));
        if members.len() > 32 {
            return None;
        }
    }
    if members.is_empty() {
        return None;
    }
    Some(members.join(","))
}

fn trim_ows(value: &str) -> &str {
    value.trim_matches(|c| c == ' ' || c == '\t')
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '*' | '/')
}

fn valid_tracestate_key(key: &str) -> bool {
    let mut parts = key.split('@');
    let Some(first) = parts.next() else {
        return false;
    };
    match parts.next() {
        None => valid_simple_key(first),
        Some(system) if parts.next().is_none() => valid_tenant_id(first) && valid_system_id(system),
        Some(_) => false,
    }
}

fn valid_simple_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && key.len() <= 256
        && chars.all(is_key_char)
}

fn valid_tenant_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 241
        && chars.all(is_key_char)
}

fn valid_system_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && id.len() <= 14
        && chars.all(is_key_char)
}

fn valid_tracestate_value(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(last) = chars.next_back() else {
        return false;
    };
    value.len() <= 256 && is_nblk(last) && chars.all(is_chr)
}

fn is_nblk(c: char) -> bool {
    matches!(c, '\u{21}'..='\u{2b}' | '\u{2d}'..='\u{3c}' | '\u{3e}'..='\u{7e}')
}

fn is_chr(c: char) -> bool {
    c == ' ' || is_nblk(c)
}

fn is_lower_hex(value: &str) -> bool {
    value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// Identifiers of the active `astra.run` span. Child spans use this as parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLink {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub flags: u8,
    pub trace_state: String,
}

/// Background `astra.run`. Drop or [`RunSpan::end`] closes it.
pub struct RunSpan {
    #[cfg(feature = "otel")]
    span: Option<opentelemetry::global::BoxedSpan>,
    error_code: Option<&'static str>,
    link: Option<RunLink>,
    owner: bool,
}

impl RunSpan {
    pub fn start(carrier: &Carrier) -> Self {
        #[cfg(feature = "otel")]
        {
            start_otel(carrier)
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = carrier;
            Self::idle()
        }
    }

    /// A child task's view of a run. Dropping it does not end the owning span.
    pub fn observe(link: Option<RunLink>) -> Self {
        Self {
            #[cfg(feature = "otel")]
            span: None,
            error_code: None,
            link,
            owner: false,
        }
    }

    /// Record a stable outcome. The description is the code, not the cause.
    pub fn fail(&mut self, code: &'static str) {
        self.error_code = Some(code);
    }

    pub fn end(&mut self) {
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::{Span, Status};
            if let Some(mut span) = self.span.take() {
                if let Some(code) = self.error_code {
                    span.set_status(Status::error(code));
                    span.set_attribute(opentelemetry::KeyValue::new("error.code", code));
                }
                span.end();
            }
        }
        self.link = None;
    }

    pub fn is_open(&self) -> bool {
        self.link.is_some()
    }

    pub fn link(&self) -> Option<RunLink> {
        self.link.clone()
    }

    fn idle() -> Self {
        Self {
            #[cfg(feature = "otel")]
            span: None,
            error_code: None,
            link: None,
            owner: true,
        }
    }
}

impl Drop for RunSpan {
    fn drop(&mut self) {
        self.end();
    }
}

/// Carrier for the tracing span that is current on this task.
pub fn current() -> Carrier {
    #[cfg(feature = "otel")]
    {
        current_otel()
    }
    #[cfg(not(feature = "otel"))]
    {
        Carrier::default()
    }
}

/// Carrier for `span`, whether or not it is the task's current span.
pub fn from_span(span: &tracing::Span) -> Carrier {
    #[cfg(feature = "otel")]
    {
        from_span_otel(span)
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = span;
        Carrier::default()
    }
}

/// Make `span` a child of the active run. No-op when export is off.
pub fn attach_link(span: &tracing::Span, link: &RunLink) {
    #[cfg(feature = "otel")]
    attach_link_otel(span, link);
    #[cfg(not(feature = "otel"))]
    {
        let _ = (span, link);
    }
}

/// Create the coarse tool span using execution metadata only. Callers supply
/// the resolved tool name and route, never tool arguments or result bodies.
pub fn tool_span(tool_name: &str, call_id: Option<&str>, route: &str) -> tracing::Span {
    let span = tracing::info_span!(
        target: "astra.tool",
        "tool.exec",
        otel.name = %format!("tool.exec {tool_name}"),
        tool.name = tool_name,
        tool.route = route,
        tool.call.id = tracing::field::Empty,
    );
    if let Some(call_id) = call_id.filter(|value| !value.is_empty()) {
        span.record("tool.call.id", call_id);
    }
    span
}

/// Stable exported status. The description is `code`, not the cause text.
pub fn mark_error(span: &tracing::Span, code: &'static str) {
    #[cfg(feature = "otel")]
    mark_error_otel(span, code);
    #[cfg(not(feature = "otel"))]
    {
        let _ = (span, code);
    }
}

tokio::task_local! {
    static ACTIVE_RUN: RefCell<RunSpan>;
    static ATTEMPT_CODE: RefCell<Option<&'static str>>;
}

/// Bind `astra.run` and a fresh attempt-code slot for this task.
///
/// The run future is large; keep the scopes directly around it. An async
/// wrapper here adds a copy of that future to every poll's stack frame.
pub fn bind_run<F>(carrier: &Carrier, future: F) -> impl Future<Output = F::Output> + use<F>
where
    F: Future,
{
    ATTEMPT_CODE.scope(
        RefCell::new(None),
        ACTIVE_RUN.scope(RefCell::new(RunSpan::start(carrier)), future),
    )
}

/// Let `future` see the caller's current run without owning it. The run is
/// captured now, so the result can be handed to any task owner (`JoinSet`,
/// `tokio::spawn`); task-locals do not follow a future onto another task.
pub fn in_active_run<F>(future: F) -> impl Future<Output = F::Output> + use<F>
where
    F: Future,
{
    let link = ACTIVE_RUN
        .try_with(|slot| slot.borrow().link())
        .ok()
        .flatten();
    ACTIVE_RUN.scope(
        RefCell::new(RunSpan::observe(link)),
        ATTEMPT_CODE.scope(RefCell::new(None), future),
    )
}

/// Run `future` on a new task that can see the current run without owning it.
pub fn spawn_in_active_run<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(in_active_run(future))
}

pub fn parent_to_active_run(span: &tracing::Span) {
    let link = ACTIVE_RUN
        .try_with(|slot| slot.borrow().link())
        .ok()
        .flatten();
    if let Some(link) = link {
        attach_link(span, &link);
    }
}

/// Only the task that owns `astra.run` may end or restart it; an observing
/// child task leaves the parent's span alone.
pub fn end_active_run() {
    let _ = ACTIVE_RUN.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.owner {
            slot.end();
        }
    });
}

pub fn reopen_active_run() {
    let _ = ACTIVE_RUN.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.owner && !slot.is_open() {
            *slot = RunSpan::start(&Carrier::default());
        }
    });
}

/// Record the run outcome. An observing child task has no span to mark.
pub fn fail_active_run(code: &'static str) {
    let _ = ACTIVE_RUN.try_with(|slot| slot.borrow_mut().fail(code));
}

/// The outcome recorded on this task's run slot, with or without export.
pub fn active_run_error_code() -> Option<&'static str> {
    ACTIVE_RUN
        .try_with(|slot| slot.borrow().error_code)
        .ok()
        .flatten()
}

pub fn note_attempt_code(code: &'static str) {
    let _ = ATTEMPT_CODE.try_with(|slot| *slot.borrow_mut() = Some(code));
}

pub fn clear_attempt_code() {
    let _ = ATTEMPT_CODE.try_with(|slot| *slot.borrow_mut() = None);
}

pub fn attempt_code() -> Option<&'static str> {
    ATTEMPT_CODE.try_with(|slot| *slot.borrow()).ok().flatten()
}

#[cfg(feature = "otel")]
fn start_otel(carrier: &Carrier) -> RunSpan {
    use opentelemetry::global;
    use opentelemetry::trace::{Span, Tracer};

    if !export_ready() {
        return RunSpan::idle();
    }
    let parent = match carrier.traceparent.as_deref() {
        Some(traceparent) => global::get_text_map_propagator(|prop| {
            prop.extract(&HeaderExtractor {
                traceparent: Some(traceparent),
                tracestate: carrier.tracestate.as_deref(),
            })
        }),
        None => opentelemetry::Context::new(),
    };
    let tracer = global::tracer("astra");
    let mut span = tracer.build_with_context(tracer.span_builder("astra.run"), &parent);
    let context = span.span_context();
    if !context.is_valid() {
        span.end();
        return RunSpan::idle();
    }
    let link = RunLink {
        trace_id: context.trace_id().to_bytes(),
        span_id: context.span_id().to_bytes(),
        flags: context.trace_flags().to_u8(),
        trace_state: context.trace_state().header(),
    };
    RunSpan {
        span: Some(span),
        error_code: None,
        link: Some(link),
        owner: true,
    }
}

#[cfg(feature = "otel")]
fn current_otel() -> Carrier {
    from_span_otel(&tracing::Span::current())
}

#[cfg(feature = "otel")]
fn from_span_otel(span: &tracing::Span) -> Carrier {
    use opentelemetry::global;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if !export_ready() {
        return Carrier::default();
    }
    let mut values = std::collections::HashMap::<String, String>::new();
    global::get_text_map_propagator(|prop| {
        prop.inject_context(&span.context(), &mut MapInjector(&mut values));
    });
    Carrier::from_header_values(
        values.get("traceparent").map(String::as_str),
        values.get("tracestate").map(String::as_str),
    )
}

#[cfg(feature = "otel")]
fn attach_link_otel(span: &tracing::Span, link: &RunLink) {
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let trace_state = if link.trace_state.is_empty() {
        TraceState::default()
    } else {
        link.trace_state.parse::<TraceState>().unwrap_or_default()
    };
    let parent = SpanContext::new(
        TraceId::from_bytes(link.trace_id),
        SpanId::from_bytes(link.span_id),
        TraceFlags::new(link.flags),
        true,
        trace_state,
    );
    if !parent.is_valid() {
        return;
    }
    let _ = span.set_parent(opentelemetry::Context::new().with_remote_span_context(parent));
}

#[cfg(feature = "otel")]
fn mark_error_otel(span: &tracing::Span, code: &'static str) {
    use opentelemetry::trace::Status;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    span.set_status(Status::error(code));
    span.set_attribute("error.code", code);
}

#[cfg(feature = "otel")]
struct HeaderExtractor<'a> {
    traceparent: Option<&'a str>,
    tracestate: Option<&'a str>,
}

#[cfg(feature = "otel")]
impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "traceparent" => self.traceparent,
            "tracestate" => self.tracestate,
            _ => None,
        }
    }

    fn keys(&self) -> Vec<&str> {
        let mut keys = Vec::new();
        if self.traceparent.is_some() {
            keys.push("traceparent");
        }
        if self.tracestate.is_some() {
            keys.push("tracestate");
        }
        keys
    }
}

#[cfg(feature = "otel")]
struct MapInjector<'a>(&'a mut std::collections::HashMap<String, String>);

#[cfg(feature = "otel")]
impl opentelemetry::propagation::Injector for MapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
}

#[cfg(test)]
mod carrier_tests {
    use super::Carrier;

    #[test]
    fn forbidden_version_is_not_a_carrier() {
        let carrier = Carrier::from_header_values(
            Some("ff-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
            Some("vendor=value"),
        );
        assert_eq!(carrier.traceparent, None);
        let carrier = Carrier::from_header_values(
            Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
            None,
        );
        assert!(carrier.traceparent.is_some());
    }

    #[test]
    fn traceparent_follows_w3c_versioning() {
        let parent = |value: &str| Carrier::from_header_values(Some(value), None).traceparent;
        let higher = "01-11111111111111111111111111111111-0123456789abcdef-01-extra";
        assert_eq!(parent(higher).as_deref(), Some(higher));
        assert!(parent("01-11111111111111111111111111111111-0123456789abcdef-01").is_some());
        assert!(parent("00-11111111111111111111111111111111-0123456789abcdef-01-extra").is_none());
        assert!(parent("01-11111111111111111111111111111111-0123456789abcdef-01x").is_none());
        assert!(parent("00-ABCDEF11111111111111111111111111-0123456789abcdef-01").is_none());
        assert!(parent("00-00000000000000000000000000000000-0123456789abcdef-01").is_none());
        assert!(parent("00-11111111111111111111111111111111-0000000000000000-01").is_none());
    }

    #[test]
    fn child_scope_observes_without_owning_the_run() {
        use super::{
            active_run_error_code, bind_run, end_active_run, fail_active_run, in_active_run,
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            bind_run(&Carrier::default(), async {
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn(in_active_run(async {
                    end_active_run();
                    fail_active_run("child_failed");
                    active_run_error_code()
                }));
                let child = tasks.join_next().await.expect("child").expect("join");
                assert_eq!(child, Some("child_failed"));
                assert_eq!(active_run_error_code(), None);
                fail_active_run("run_admission_timeout");
                assert_eq!(active_run_error_code(), Some("run_admission_timeout"));
            })
            .await;
        });
        assert_eq!(active_run_error_code(), None);
    }

    #[test]
    fn tracestate_keeps_optional_whitespace_and_repeated_fields() {
        let parent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        let carrier = Carrier::from_header_values(
            Some(parent),
            Some("rojo=00f067aa0ba902b7, congo=t61rcWkgMzE"),
        );
        assert!(carrier.traceparent.is_some());
        assert_eq!(
            carrier.tracestate.as_deref(),
            Some("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE")
        );
        let spaced = Carrier::from_header_values(Some(parent), Some("vendor=a b"));
        assert_eq!(spaced.tracestate.as_deref(), Some("vendor=a b"));
        let invalid_value = Carrier::from_header_values(Some(parent), Some("vendor=a=b"));
        assert!(invalid_value.traceparent.is_some());
        assert_eq!(invalid_value.tracestate, None);
        let blank_value = Carrier::from_header_values(Some(parent), Some("vendor= "));
        assert!(blank_value.traceparent.is_some());
        assert_eq!(blank_value.tracestate, None);
        let repeated = Carrier::from_header_fields(
            Some(parent),
            ["rojo=00f067aa0ba902b7", "congo=t61rcWkgMzE"],
        );
        assert_eq!(
            repeated.tracestate.as_deref(),
            Some("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE")
        );
        let tenant = Carrier::from_header_values(Some(parent), Some("congo@rojo=t61rcWkgMzE"));
        assert_eq!(tenant.tracestate.as_deref(), Some("congo@rojo=t61rcWkgMzE"));
    }
}

#[cfg(all(test, feature = "otel"))]
mod tests {
    use super::*;
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use std::sync::{Arc, Mutex};
    use tracing_opentelemetry::OpenTelemetryLayer;
    use tracing_subscriber::{EnvFilter, layer::Layer, prelude::*};

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[derive(Clone, Debug)]
    struct Capture(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Capture {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            self.0.lock().expect("span capture").extend(batch);
            async { Ok(()) }
        }
    }

    fn capture_provider() -> (Capture, SdkTracerProvider) {
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(capture.clone())
            .build();
        (capture, provider)
    }

    #[test]
    fn tool_spans_export_identity_route_and_parent_without_arguments() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (exporter, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();
        let subscriber = tracing_subscriber::registry().with(
            OpenTelemetryLayer::new(provider.tracer("astra-test"))
                .with_filter(EnvFilter::new("astra.tool=info")),
        );
        let _default = tracing::subscriber::set_default(subscriber);
        let mut run = RunSpan::start(&Carrier::default());
        let link = run.link().expect("run link");

        for (name, call_id, route) in [
            ("bash", Some("call-1"), "edge_bound"),
            ("mcp_lookup", None, "request_scoped_mcp"),
            ("web_fetch", Some(""), "server_local"),
        ] {
            let span = tool_span(name, call_id, route);
            attach_link(&span, &link);
            {
                let _entered = span.enter();
                tracing::warn!(target: "astra_runtime::tool", command = "secret-command", "private output");
            }
            if name == "bash" {
                mark_error(&span, "stream_transport");
            }
            drop(span);
        }
        run.end();

        let finished = exporter.0.lock().expect("span capture");
        for (name, call_id, route) in [
            ("bash", Some("call-1"), "edge_bound"),
            ("mcp_lookup", None, "request_scoped_mcp"),
            ("web_fetch", None, "server_local"),
        ] {
            let span = finished
                .iter()
                .find(|span| span.name == format!("tool.exec {name}"))
                .expect("tool span exported");
            assert_eq!(span.parent_span_id.to_bytes(), link.span_id);
            for (key, value) in [
                ("tool.name", Some(name)),
                ("tool.route", Some(route)),
                ("tool.call.id", call_id),
            ] {
                let actual = span
                    .attributes
                    .iter()
                    .find(|attr| attr.key.as_str() == key)
                    .map(|attr| attr.value.as_str().into_owned());
                assert_eq!(actual.as_deref(), value);
            }
            let rendered = format!("{span:?}");
            assert!(!rendered.contains("secret-command"));
            assert!(!rendered.contains("private output"));
            if name == "bash" {
                assert_eq!(
                    span.status,
                    opentelemetry::trace::Status::error("stream_transport")
                );
            }
        }
    }

    #[test]
    fn export_omits_llm_client_url_and_error() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (exporter, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();

        let tracer = provider.tracer("astra-test");
        let filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
        let subscriber = tracing_subscriber::registry()
            .with(OpenTelemetryLayer::new(tracer).with_filter(filter));
        let _default = tracing::subscriber::set_default(subscriber);

        let secret_url = "https://model.example/v1/chat?key=secret";
        let secret_error = "connection refused to https://model.example/private";
        let span = tracing::info_span!(target: "astra.llm", "llm.call");
        {
            let _entered = span.enter();
            tracing::warn!(
                target: "astra_runtime::llm_client",
                url = %secret_url,
                error = %secret_error,
                "LLM send failed"
            );
            mark_error(&tracing::Span::current(), "network");
        }
        drop(span);

        let finished = exporter.0.lock().expect("span capture").clone();
        let call = finished
            .iter()
            .find(|span| span.name == "llm.call")
            .expect("llm.call exported");
        let rendered = format!("{call:?}");
        assert!(!rendered.contains(secret_url), "{rendered}");
        assert!(!rendered.contains("connection refused"), "{rendered}");
        assert!(!rendered.contains("private"), "{rendered}");
        assert!(
            call.events.iter().all(|event| {
                let text = format!("{event:?}");
                !text.contains(secret_url) && !text.contains("connection refused")
            }),
            "{:?}",
            call.events
        );
        assert_eq!(call.status, opentelemetry::trace::Status::error("network"));
        assert!(
            call.attributes.iter().any(|attribute| {
                attribute.key.as_str() == "error.code" && attribute.value.as_str() == "network"
            }),
            "{:?}",
            call.attributes
        );
    }

    #[test]
    fn run_span_uses_inbound_trace_and_ends() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (exporter, provider) = capture_provider();
        global::set_tracer_provider(provider);
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();

        let carrier = Carrier::from_header_values(
            Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
            None,
        );
        let mut run = RunSpan::start(&carrier);
        let link = run.link().expect("run link");
        assert_eq!(
            link.trace_id,
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
        run.end();
        let finished = exporter.0.lock().expect("span capture").clone();
        assert!(finished.iter().any(|span| span.name == "astra.run"));
    }

    #[test]
    fn llm_call_is_a_child_of_the_run_span() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (exporter, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();

        let tracer = provider.tracer("astra-test");
        let filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
        let subscriber = tracing_subscriber::registry()
            .with(OpenTelemetryLayer::new(tracer).with_filter(filter));
        let _default = tracing::subscriber::set_default(subscriber);

        let mut run = RunSpan::start(&Carrier::default());
        let link = run.link().expect("run link");
        let span = tracing::info_span!(target: "astra.llm", "llm.call");
        attach_link(&span, &link);
        {
            let _entered = span.enter();
        }
        drop(span);
        run.end();

        let finished = exporter.0.lock().expect("span capture").clone();
        let call = finished
            .iter()
            .find(|span| span.name == "llm.call")
            .expect("llm.call exported");
        assert_eq!(call.parent_span_id.to_bytes(), link.span_id);
    }

    #[test]
    fn child_span_keeps_inbound_tracestate() {
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        let _guard = TEST_LOCK.lock().unwrap();
        let (_, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();
        let tracer = provider.tracer("astra-test");
        let filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
        let subscriber = tracing_subscriber::registry()
            .with(OpenTelemetryLayer::new(tracer).with_filter(filter));
        let _default = tracing::subscriber::set_default(subscriber);

        let carrier = Carrier::from_header_values(
            Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
            Some("vendor=value"),
        );
        let run = RunSpan::start(&carrier);
        let link = run.link().expect("run link");
        assert!(
            link.trace_state.contains("vendor=value"),
            "{}",
            link.trace_state
        );
        let span = tracing::info_span!(target: "astra.llm", "llm.call");
        attach_link(&span, &link);
        let _entered = span.enter();
        let mut values = std::collections::HashMap::<String, String>::new();
        global::get_text_map_propagator(|prop| {
            prop.inject_context(&span.context(), &mut super::MapInjector(&mut values));
        });
        let state = values.get("tracestate").map(String::as_str).unwrap_or("");
        assert!(state.contains("vendor=value"), "{values:?}");
    }

    #[test]
    fn spawned_calls_share_the_run_and_record_outcomes() {
        use opentelemetry::trace::Status;

        let _guard = TEST_LOCK.lock().unwrap();
        let (exporter, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();
        let tracer = provider.tracer("astra-test");
        let filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
        let subscriber = tracing_subscriber::registry()
            .with(OpenTelemetryLayer::new(tracer).with_filter(filter));
        let _default = tracing::subscriber::set_default(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");

        let parent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        runtime.block_on(async {
            bind_run(&Carrier::from_header_values(Some(parent), None), async {
                let llm = spawn_in_active_run(async {
                    let span = tracing::info_span!(target: "astra.llm", "llm.call");
                    parent_to_active_run(&span);
                    drop(span);
                });
                let denied = spawn_in_active_run(async {
                    let span = tracing::info_span!(target: "astra.tool", "tool.exec");
                    parent_to_active_run(&span);
                    mark_error(&span, "policy_denied");
                    drop(span);
                });
                let ok_tool = spawn_in_active_run(async {
                    let span = tracing::info_span!(target: "astra.tool", "tool.exec");
                    parent_to_active_run(&span);
                    drop(span);
                });
                llm.await.expect("llm task");
                denied.await.expect("denied tool task");
                ok_tool.await.expect("tool task");
            })
            .await;
            bind_run(&Carrier::default(), async {
                fail_active_run("provider_deadline");
            })
            .await;
        });

        let finished = exporter.0.lock().expect("span capture").clone();
        let success = finished
            .iter()
            .find(|span| span.name == "astra.run" && span.status == Status::Unset)
            .expect("successful run");
        let children: Vec<_> = finished
            .iter()
            .filter(|span| span.name == "llm.call" || span.name == "tool.exec")
            .collect();
        assert_eq!(children.len(), 3);
        for child in &children {
            assert_eq!(
                child.span_context.trace_id(),
                success.span_context.trace_id()
            );
            assert_eq!(child.parent_span_id, success.span_context.span_id());
        }
        assert!(
            children
                .iter()
                .any(|span| span.name == "llm.call" && span.status == Status::Unset)
        );
        assert!(children.iter().any(|span| {
            span.name == "tool.exec"
                && span.status == Status::error("policy_denied")
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "error.code"
                        && attribute.value.as_str() == "policy_denied"
                })
        }));
        let failed = finished
            .iter()
            .find(|span| {
                span.name == "astra.run" && span.status == Status::error("provider_deadline")
            })
            .expect("failed run");
        assert!(failed.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "error.code"
                && attribute.value.as_str() == "provider_deadline"
        }));
    }

    #[test]
    fn joinset_child_joins_the_run_only_through_the_scope() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (exporter, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();
        let tracer = provider.tracer("astra-test");
        let filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
        let subscriber = tracing_subscriber::registry()
            .with(OpenTelemetryLayer::new(tracer).with_filter(filter));
        let _default = tracing::subscriber::set_default(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");

        let parent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        let call = |name: &'static str| async move {
            let span = tracing::info_span!(target: "astra.llm", "llm.call", probe = name);
            parent_to_active_run(&span);
            // A child that parks on user input must not end the parent span.
            end_active_run();
            reopen_active_run();
            drop(span);
        };
        runtime.block_on(async {
            bind_run(&Carrier::from_header_values(Some(parent), None), async {
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn(in_active_run(call("scoped")));
                tasks.spawn(call("unscoped"));
                while let Some(joined) = tasks.join_next().await {
                    joined.expect("child task");
                }
            })
            .await;
        });

        let finished = exporter.0.lock().expect("span capture").clone();
        let runs: Vec<_> = finished
            .iter()
            .filter(|span| span.name == "astra.run")
            .collect();
        assert_eq!(
            runs.len(),
            1,
            "an observing child must not open another run"
        );
        let run = runs[0];
        let probe = |name: &str| {
            finished
                .iter()
                .find(|span| {
                    span.name == "llm.call"
                        && span.attributes.iter().any(|attribute| {
                            attribute.key.as_str() == "probe" && attribute.value.as_str() == name
                        })
                })
                .expect("llm.call exported")
        };
        let scoped = probe("scoped");
        assert_eq!(scoped.span_context.trace_id(), run.span_context.trace_id());
        assert_eq!(scoped.parent_span_id, run.span_context.span_id());
        let unscoped = probe("unscoped");
        assert_ne!(
            unscoped.span_context.trace_id(),
            run.span_context.trace_id()
        );
    }

    #[test]
    fn instrumented_llm_attempt_injects_the_run_traceparent() {
        use tracing::Instrument;

        let _guard = TEST_LOCK.lock().unwrap();
        let (_, provider) = capture_provider();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        mark_export_ready();
        let tracer = provider.tracer("astra-test");
        let filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
        let subscriber = tracing_subscriber::registry()
            .with(OpenTelemetryLayer::new(tracer).with_filter(filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(EnvFilter::new("error")),
            );
        let _default = tracing::subscriber::set_default(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");

        let parent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        let trace_id = "0123456789abcdef0123456789abcdef";
        runtime.block_on(async {
            bind_run(&Carrier::from_header_values(Some(parent), None), async {
                let span = tracing::info_span!(target: "astra.llm", "llm.call");
                parent_to_active_run(&span);
                {
                    let _entered = span.enter();
                }
                let outside = current();
                let send_request = async { current() };
                let send_request = send_request.instrument(span.clone());
                let inside = tokio::select! {
                    biased;
                    _ = std::future::pending::<()>() => unreachable!(),
                    result = tokio::time::timeout(std::time::Duration::from_secs(2), send_request) => {
                        result.expect("instrumented send should finish")
                    }
                };
                let direct = from_span(&span);
                assert_eq!(
                    outside.traceparent.as_deref(),
                    None,
                    "inside={inside:?} direct={direct:?}"
                );
                let inside_parent = inside.traceparent.as_deref().unwrap_or("");
                assert!(
                    inside_parent.contains(trace_id) && valid_traceparent(inside_parent),
                    "inside={inside:?} direct={direct:?} outside={outside:?}"
                );
                assert!(
                    direct.traceparent.as_deref().unwrap_or("").contains(trace_id),
                    "direct={direct:?}"
                );
            })
            .await;
        });
    }
}
