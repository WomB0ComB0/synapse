//! Telemetry: structured JSON logs plus optional OTLP traces and metrics.

use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};
use opentelemetry::KeyValue;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Flushes and shuts down the OTLP providers on drop.
///
/// Hold it for the lifetime of the process so buffered telemetry is exported before exit. When no
/// OTLP endpoint is configured both fields are empty and `Drop` is a no-op.
pub struct OtelGuard {
    trace_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    meter_provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Stop metrics first so no periodic export races the trace shutdown during process exit.
        if let Some(provider) = self.meter_provider.take() {
            if let Err(error) = provider.shutdown() {
                eprintln!("otel meter shutdown error: {error}");
            }
        }
        if let Some(provider) = self.trace_provider.take() {
            if let Err(error) = provider.shutdown() {
                eprintln!("otel tracer shutdown error: {error}");
            }
        }
    }
}

/// Low-cardinality HTTP server metrics shared by the request middleware.
#[derive(Clone)]
pub struct HttpMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
    active: UpDownCounter<i64>,
}

impl HttpMetrics {
    /// Construct instruments from the process-wide meter provider. When OTLP is disabled these are
    /// no-op instruments, so test and development routers need no conditional wiring.
    #[must_use]
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("synapse");
        Self {
            requests: meter
                .u64_counter("synapse.http.server.requests")
                .with_description("Completed inbound HTTP requests")
                .with_unit("{request}")
                .build(),
            duration: meter
                .f64_histogram("http.server.request.duration")
                .with_description("Inbound HTTP request duration")
                .with_unit("s")
                .with_boundaries(vec![
                    0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 0.750, 1.0, 2.5, 5.0, 10.0,
                    30.0, 60.0, 180.0,
                ])
                .build(),
            active: meter
                .i64_up_down_counter("http.server.active_requests")
                .with_description("Currently active inbound HTTP requests")
                .with_unit("{request}")
                .build(),
        }
    }
}

impl Default for HttpMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Record request count, latency, status, and concurrency around all router admission controls.
///
/// Labels are deliberately bounded to method, the matched route template, and numeric status. Raw
/// paths, tenant ids, principal ids, document ids, and tool arguments never become metric labels.
pub async fn record_http_metrics(
    State(metrics): State<HttpMetrics>,
    request: Request,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();
    let method = request.method().as_str().to_owned();
    let active_attributes = vec![
        KeyValue::new("http.request.method", method.clone()),
        KeyValue::new("http.route", route.clone()),
    ];
    metrics.active.add(1, &active_attributes);
    let in_flight = InFlightRequest {
        active: metrics.active.clone(),
        attributes: active_attributes,
    };
    let started = Instant::now();

    let response = next.run(request).await;
    let elapsed = started.elapsed().as_secs_f64();
    drop(in_flight);

    let attributes = [
        KeyValue::new("http.request.method", method),
        KeyValue::new("http.route", route),
        KeyValue::new(
            "http.response.status_code",
            i64::from(response.status().as_u16()),
        ),
    ];
    metrics.requests.add(1, &attributes);
    metrics.duration.record(elapsed, &attributes);
    response
}

/// RAII decrement so cancellation or panic cannot leave the active-request metric inflated.
struct InFlightRequest {
    active: UpDownCounter<i64>,
    attributes: Vec<KeyValue>,
}

impl Drop for InFlightRequest {
    fn drop(&mut self) {
        self.active.add(-1, &self.attributes);
    }
}

/// Initialize process-wide tracing and metrics.
///
/// Structured JSON logs (controlled by `RUST_LOG`, default `info,synapse=debug`) are always
/// installed. When `otel_endpoint` is set, OTLP HTTP/protobuf exporters send traces and metrics to
/// the signal-specific paths below that base URL. A failure in either signal degrades independently:
/// logs and the other successfully initialized signal remain active.
///
/// Returns an [`OtelGuard`] whose `Drop` flushes exported telemetry; keep it alive until exit.
pub fn init(otel_endpoint: Option<&str>) -> OtelGuard {
    let otel_endpoint = otel_endpoint
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,synapse=debug"));
    let fmt_layer = fmt::layer().json().with_target(true);

    let trace_provider = otel_endpoint.and_then(|endpoint| match build_trace_provider(endpoint) {
        Ok(provider) => {
            opentelemetry::global::set_tracer_provider(provider.clone());
            Some(provider)
        }
        Err(error) => {
            eprintln!("failed to initialize OTLP trace exporter ({error}); continuing without it");
            None
        }
    });
    let meter_provider = otel_endpoint.and_then(|endpoint| match build_meter_provider(endpoint) {
        Ok(provider) => {
            opentelemetry::global::set_meter_provider(provider.clone());
            Some(provider)
        }
        Err(error) => {
            eprintln!("failed to initialize OTLP metric exporter ({error}); continuing without it");
            None
        }
    });

    let otel_layer = trace_provider.as_ref().map(|provider| {
        use opentelemetry::trace::TracerProvider as _;
        tracing_opentelemetry::layer().with_tracer(provider.tracer("synapse"))
    });

    // `Option<Layer>` is a no-op when absent. `try_init` makes repeated initialization in tests
    // harmless instead of panicking.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init();

    OtelGuard {
        trace_provider,
        meter_provider,
    }
}

/// Build an OTLP/HTTP protobuf span exporter and batch tracer provider.
fn build_trace_provider(
    endpoint: &str,
) -> anyhow::Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry_otlp::WithExportConfig as _;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(endpoint, "traces")?)
        .build()?;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(service_resource())
        .build();
    Ok(provider)
}

/// Build an OTLP/HTTP protobuf metric exporter and periodic meter provider.
fn build_meter_provider(
    endpoint: &str,
) -> anyhow::Result<opentelemetry_sdk::metrics::SdkMeterProvider> {
    use opentelemetry_otlp::WithExportConfig as _;

    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(endpoint, "metrics")?)
        .build()?;
    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(service_resource())
        .build();
    Ok(provider)
}

fn service_resource() -> opentelemetry_sdk::Resource {
    opentelemetry_sdk::Resource::builder()
        .with_service_name("synapse")
        .build()
}

/// Resolve a generic OTLP HTTP base endpoint to a signal-specific endpoint.
///
/// Standard `OTEL_EXPORTER_OTLP_ENDPOINT` semantics append `/v1/<signal>`. Accepting and replacing
/// an existing signal suffix preserves compatibility with the previous trace-only configuration.
fn signal_endpoint(base: &str, signal: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(base)?;
    let mut path = url.path().trim_end_matches('/').to_owned();
    for suffix in ["/v1/traces", "/v1/metrics", "/v1/logs"] {
        if let Some(prefix) = path.strip_suffix(suffix) {
            path = prefix.to_owned();
            break;
        }
    }
    url.set_path(&format!("{path}/v1/{signal}"));
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_endpoint_is_a_noop() {
        let guard = init(None);
        assert!(guard.trace_provider.is_none());
        assert!(guard.meter_provider.is_none());
    }

    #[test]
    fn generic_endpoint_resolves_each_signal_path() {
        assert_eq!(
            signal_endpoint("http://127.0.0.1:4318", "traces").unwrap(),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            signal_endpoint("https://otel.example.com/collector/", "metrics").unwrap(),
            "https://otel.example.com/collector/v1/metrics"
        );
        assert_eq!(
            signal_endpoint("http://127.0.0.1:4318/v1/traces", "metrics").unwrap(),
            "http://127.0.0.1:4318/v1/metrics"
        );
    }

    #[test]
    fn dummy_endpoint_builds_and_flushes_trace_and_metric_providers() {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry::trace::{Tracer as _, TracerProvider as _};

        let trace_provider =
            build_trace_provider("http://127.0.0.1:4318").expect("builds trace provider");
        let tracer = trace_provider.tracer("test");
        tracer.in_span("exercise-export", |_context| {});
        let _ = trace_provider.force_flush();
        let _ = trace_provider.shutdown();

        let meter_provider =
            build_meter_provider("http://127.0.0.1:4318").expect("builds meter provider");
        let meter = meter_provider.meter("test");
        meter.u64_counter("test.counter").build().add(1, &[]);
        let _ = meter_provider.force_flush();
        let _ = meter_provider.shutdown();
    }
}
