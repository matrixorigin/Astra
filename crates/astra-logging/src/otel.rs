//! OTLP trace export (feature `otel`). Enabled only when `OTEL_TRACES_ENABLED=1` and an OTLP endpoint is set.

use std::sync::{Mutex, OnceLock};

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::{
    Resource,
    resource::{EnvResourceDetector, SdkProvidedResourceDetector},
    trace::{SdkTracer, SdkTracerProvider},
};
use tracing_subscriber::{
    EnvFilter, fmt::time::UtcTime, layer::Layer, prelude::*, registry::Registry,
    util::SubscriberInitExt,
};

use crate::{InitError, LogFormat, LogInitConfig, resolve_format};

static TRACER_PROVIDER: OnceLock<Mutex<Option<SdkTracerProvider>>> = OnceLock::new();

fn tracer_provider_slot() -> &'static Mutex<Option<SdkTracerProvider>> {
    TRACER_PROVIDER.get_or_init(|| Mutex::new(None))
}

/// Export only when the shared switch and an unpadded `OTEL_EXPORTER_OTLP_ENDPOINT` are both set.
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` alone, a padded endpoint, or `ASTRA_OTEL_ENABLED=1` alone does not enable export.
pub(crate) fn wants_otel_export() -> bool {
    if std::env::var("OTEL_TRACES_ENABLED").ok().as_deref() != Some("1") {
        return false;
    }
    fn accepted(name: &str) -> bool {
        std::env::var(name)
            .ok()
            .is_some_and(|value| endpoint_accepted(&value))
    }
    accepted("OTEL_EXPORTER_OTLP_ENDPOINT")
}

fn endpoint_accepted(value: &str) -> bool {
    !value.is_empty() && value == value.trim()
}

fn build_resource(config: &LogInitConfig<'_>) -> Resource {
    let mut builder = Resource::builder_empty()
        .with_detector(Box::new(SdkProvidedResourceDetector))
        .with_detector(Box::new(EnvResourceDetector::new()));
    if let Some(name) = config.service_name.filter(|s| !s.is_empty()) {
        builder = builder.with_service_name(name.to_string());
    } else if let Ok(name) = std::env::var("ASTRA_SERVICE_NAME")
        && !name.trim().is_empty()
    {
        builder = builder.with_service_name(name);
    }
    builder.build()
}

/// Install OTLP batch exporter, register the global tracer provider, and return an SDK tracer for `tracing-opentelemetry`.
fn install_otlp_tracer(config: &LogInitConfig<'_>) -> Result<SdkTracer, InitError> {
    let resource = build_resource(config);
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .map_err(|e| -> InitError { Box::new(e) })?;

    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();
    let tracer = provider.tracer("astra");
    global::set_tracer_provider(provider.clone());
    if let Ok(mut slot) = tracer_provider_slot().lock() {
        *slot = Some(provider);
    }
    Ok(tracer)
}

fn make_filter(config: &LogInitConfig<'_>) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(config.default_filter))
}

/// `OpenTelemetryLayer` stacks on [`Registry`]; apply the same [`EnvFilter`] to otel + fmt via per-layer filtering.
fn registry_with_fmt<L>(
    config: &LogInitConfig<'_>,
    format: LogFormat,
    otel_layer: L,
) -> Result<(), InitError>
where
    L: Layer<Registry> + Send + Sync + 'static,
{
    let filter = make_filter(config);
    // stderr keeps RUST_LOG. Export accepts only the spans this feature creates.
    // Existing llm_client logs include url and error and must not become OTel events.
    let otel_filter = EnvFilter::new("astra.run=info,astra.llm=info,astra.tool=info");
    let timer = UtcTime::rfc_3339();

    match format {
        LogFormat::Json => Registry::default()
            .with(otel_layer.with_filter(otel_filter.clone()))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(timer)
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .json()
                    .with_filter(filter),
            )
            .try_init()
            .map_err(|e| -> InitError { Box::new(e) }),
        LogFormat::Pretty => Registry::default()
            .with(otel_layer.with_filter(otel_filter.clone()))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(timer)
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .pretty()
                    .with_filter(filter),
            )
            .try_init()
            .map_err(|e| -> InitError { Box::new(e) }),
        LogFormat::Compact => Registry::default()
            .with(otel_layer.with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(timer)
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .compact()
                    .with_filter(filter),
            )
            .try_init()
            .map_err(|e| -> InitError { Box::new(e) }),
    }
}

/// Install Registry + OpenTelemetry + fmt with matching env filters. Call [`crate::shutdown_otel`] on graceful exit when possible.
pub(crate) fn init_with_otel(config: &LogInitConfig<'_>) -> Result<(), InitError> {
    let format = resolve_format();
    let tracer = install_otlp_tracer(config)?;
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let res = registry_with_fmt(config, format, otel_layer);

    if res.is_ok() {
        crate::trace_context::mark_export_ready();
        let name = std::env::var("ASTRA_SERVICE_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| config.service_name.map(str::to_owned));
        if let Some(ref svc) = name {
            tracing::info!(
                target: "astra.logging",
                service_name = %svc,
                "logging initialized (OTLP + stderr)"
            );
        } else {
            tracing::info!(target: "astra.logging", "logging initialized (OTLP + stderr)");
        }
    } else {
        shutdown_tracer_provider();
    }

    res
}

#[cfg(test)]
mod tests {
    use super::endpoint_accepted;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvReset;

    impl Drop for EnvReset {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("OTEL_TRACES_ENABLED");
                std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
                std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
            }
        }
    }

    #[test]
    fn padded_or_empty_endpoint_is_rejected() {
        assert!(!endpoint_accepted(""));
        assert!(!endpoint_accepted(" http://127.0.0.1:4317"));
        assert!(!endpoint_accepted("http://127.0.0.1:4317 "));
        assert!(endpoint_accepted("http://127.0.0.1:4317"));
    }

    #[test]
    fn only_the_generic_endpoint_enables_export() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _reset = EnvReset;
        unsafe {
            std::env::set_var("OTEL_TRACES_ENABLED", "1");
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::set_var(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://127.0.0.1:4317",
            );
        }
        assert!(!super::wants_otel_export());
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", " http://127.0.0.1:4317");
        }
        assert!(!super::wants_otel_export());
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317");
        }
        assert!(super::wants_otel_export());
    }
}

/// Flush and shutdown the global OpenTelemetry tracer provider.
pub(crate) fn shutdown_tracer_provider() {
    let Some(provider) = tracer_provider_slot()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
    else {
        return;
    };
    if let Err(err) = provider.shutdown() {
        eprintln!("[astra-logging] OTLP shutdown failed: {err}");
    }
}
