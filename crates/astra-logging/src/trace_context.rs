//! W3C `traceparent` / `tracestate` for Astra export.
//!
//! The process exports only after OTLP init succeeds. Until then these helpers
//! carry nothing and outbound callers strip stale trace headers.

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
        Self {
            traceparent: traceparent
                .map(str::trim)
                .filter(|value| valid_traceparent(value))
                .map(str::to_owned),
            tracestate: tracestate
                .map(str::trim)
                .filter(|value| valid_tracestate(value))
                .map(str::to_owned),
        }
    }
}

pub(crate) fn valid_traceparent(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(version) = parts.next() else {
        return false;
    };
    let Some(trace_id) = parts.next() else {
        return false;
    };
    let Some(span_id) = parts.next() else {
        return false;
    };
    let Some(flags) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && version.len() == 2
        && trace_id.len() == 32
        && span_id.len() == 16
        && flags.len() == 2
        && is_hex(version)
        && !version.eq_ignore_ascii_case("ff")
        && is_hex(trace_id)
        && is_hex(span_id)
        && is_hex(flags)
        && trace_id.chars().any(|c| c != '0')
        && span_id.chars().any(|c| c != '0')
}

pub(crate) fn valid_tracestate(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value
            .chars()
            .all(|c| c.is_ascii() && !c.is_ascii_control() && c != ' ')
}

fn is_hex(value: &str) -> bool {
    value.chars().all(|c| c.is_ascii_hexdigit())
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
    link: Option<RunLink>,
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
            Self { link: None }
        }
    }

    pub fn end(&mut self) {
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::Span;
            if let Some(mut span) = self.span.take() {
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

/// Make `span` a child of the active run. No-op when export is off.
pub fn attach_link(span: &tracing::Span, link: &RunLink) {
    #[cfg(feature = "otel")]
    attach_link_otel(span, link);
    #[cfg(not(feature = "otel"))]
    {
        let _ = (span, link);
    }
}

/// Stable exported status. The description is the code `error`, not the cause.
pub fn mark_error(span: &tracing::Span) {
    #[cfg(feature = "otel")]
    mark_error_otel(span);
    #[cfg(not(feature = "otel"))]
    {
        let _ = span;
    }
}

#[cfg(feature = "otel")]
fn start_otel(carrier: &Carrier) -> RunSpan {
    use opentelemetry::global;
    use opentelemetry::trace::{Span, Tracer};

    if !export_ready() {
        return RunSpan {
            span: None,
            link: None,
        };
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
        return RunSpan {
            span: None,
            link: None,
        };
    }
    let link = RunLink {
        trace_id: context.trace_id().to_bytes(),
        span_id: context.span_id().to_bytes(),
        flags: context.trace_flags().to_u8(),
        trace_state: context.trace_state().header(),
    };
    RunSpan {
        span: Some(span),
        link: Some(link),
    }
}

#[cfg(feature = "otel")]
fn current_otel() -> Carrier {
    use opentelemetry::global;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if !export_ready() {
        return Carrier::default();
    }
    let mut values = std::collections::HashMap::<String, String>::new();
    global::get_text_map_propagator(|prop| {
        prop.inject_context(
            &tracing::Span::current().context(),
            &mut MapInjector(&mut values),
        );
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
fn mark_error_otel(span: &tracing::Span) {
    use opentelemetry::trace::Status;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    span.set_status(Status::error("error"));
    span.set_attribute("error.code", "error");
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
            mark_error(&tracing::Span::current());
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
        assert_eq!(call.status, opentelemetry::trace::Status::error("error"));
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
}
