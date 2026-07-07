//! OTLP trace export (feature `otel`). Enabled when `ASTRA_OTEL_ENABLED=1` or OTLP endpoint env is set.

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

/// Whether OTLP export should be activated (OTel feature compiled in).
pub(crate) fn wants_otel_export() -> bool {
    if std::env::var("ASTRA_OTEL_ENABLED").ok().as_deref() == Some("1") {
        return true;
    }
    fn nonempty(name: &str) -> bool {
        std::env::var(name)
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
    nonempty("OTEL_EXPORTER_OTLP_ENDPOINT") || nonempty("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
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
    let timer = UtcTime::rfc_3339();

    match format {
        LogFormat::Json => Registry::default()
            .with(otel_layer.with_filter(filter.clone()))
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
            .with(otel_layer.with_filter(filter.clone()))
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
            .with(otel_layer.with_filter(filter.clone()))
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
    }

    res
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
