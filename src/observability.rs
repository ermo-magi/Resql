//! Optional OpenTelemetry trace export over OTLP.
//!
//! Export is off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set. With it off,
//! nothing in this module touches the request path and log output is
//! unchanged. With it on:
//!
//! - every request becomes a SERVER span parented on the inbound W3C
//!   `traceparent` (see `server::request_observability`);
//! - every saved-query execution becomes a CLIENT span (see
//!   [`db_span`]);
//! - only Resql's own spans are exported. Log events (including driver
//!   events that can carry SQL text) stay in the log pipeline and
//!   correlate through the `trace_id` field; dependency spans are dropped.
//!
//! Environment contract (standard OTel variables):
//!
//! | Variable | Effect |
//! |---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | Enables export. gRPC: `http://host:4317`; HTTP: `http(s)://host:4318` |
//! | `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` / `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` (default) or `http/protobuf` |
//! | `OTEL_EXPORTER_OTLP_HEADERS` | Extra exporter headers, e.g. `authorization=Bearer%20<token>` |
//! | `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in ms |
//! | `OTEL_SERVICE_NAME` | `service.name` (default `resql`) |
//! | `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes |
//!
//! Reference: `book/src/logging.md` ("Trace export (OTLP)").

use crate::db::Pool;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator, runtime::Tokio, trace::TracerProvider, Resource,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::Layer;

/// Default `service.name` when `OTEL_SERVICE_NAME` is unset.
pub const SERVICE_NAME_DEFAULT: &str = "resql";

/// Set once [`init_provider_from_env`] has wired an exporter. The request
/// path reads it to decide whether to create or populate OTel-only spans
/// and fields. When it is `false` those fields stay empty and DB spans are
/// not created, so log output is unchanged.
static OTEL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// `true` when spans are exported over OTLP.
pub fn otel_active() -> bool {
    OTEL_ACTIVE.load(Ordering::Relaxed)
}

/// Test hook: integration tests install their own subscriber with an
/// in-memory exporter and flip this on, instead of going through
/// [`init_provider_from_env`].
#[doc(hidden)]
pub fn set_otel_active_for_tests(active: bool) {
    OTEL_ACTIVE.store(active, Ordering::Relaxed);
}

/// OTLP transport selected by `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` /
/// `OTEL_EXPORTER_OTLP_PROTOCOL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpTransport {
    /// OTLP/gRPC (default, port 4317). Plaintext only.
    Grpc,
    /// OTLP/HTTP with protobuf payloads (port 4318, `<endpoint>/v1/traces`).
    HttpProtobuf,
}

/// Resolve the OTLP transport from the standard env vars. The per-signal
/// variable wins. Unknown values fall back to gRPC with a warning on stderr
/// (the subscriber is not installed yet when this runs).
pub fn otlp_transport_from_env() -> OtlpTransport {
    let raw = std::env::var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"))
        .unwrap_or_default();
    match raw.trim() {
        "" | "grpc" => OtlpTransport::Grpc,
        "http/protobuf" => OtlpTransport::HttpProtobuf,
        other => {
            eprintln!(
                "OTEL_EXPORTER_OTLP_PROTOCOL={other:?} is not supported (grpc, http/protobuf); using grpc"
            );
            OtlpTransport::Grpc
        }
    }
}

/// Resource for exported spans: the SDK defaults (which include
/// `OTEL_RESOURCE_ATTRIBUTES` via the env detector) merged with
/// `service.name` / `service.version` from this process, which win.
pub fn otel_resource(service_name: &str) -> Resource {
    Resource::default().merge(&Resource::new(vec![
        KeyValue::new("service.name", service_name.to_string()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]))
}

/// Build the OTLP span exporter for `endpoint` with the transport from
/// [`otlp_transport_from_env`].
///
/// TLS is provided by OTLP/HTTP through `reqwest` + rustls; the gRPC
/// exporter is plaintext. An `https://` endpoint with gRPC therefore warns
/// once and is expected to fail on export.
pub fn build_span_exporter(endpoint: &str) -> Result<opentelemetry_otlp::SpanExporter, String> {
    // Endpoint, OTEL_EXPORTER_OTLP_HEADERS and OTEL_EXPORTER_OTLP_TIMEOUT are
    // read by opentelemetry-otlp itself; header values are never logged here.
    match otlp_transport_from_env() {
        OtlpTransport::Grpc => {
            if endpoint.starts_with("https://") {
                eprintln!(
                    "OTEL_EXPORTER_OTLP_ENDPOINT is https:// but the gRPC exporter has no TLS; \
                     set OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf for a TLS endpoint"
                );
            }
            opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()
                .map_err(|e| e.to_string())
        }
        OtlpTransport::HttpProtobuf => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(endpoint)
            .with_http_client(reqwest::Client::new())
            .build()
            .map_err(|e| e.to_string()),
    }
}

/// Build the tracer provider when `OTEL_EXPORTER_OTLP_ENDPOINT` is set and
/// non-empty. Returns `None` (export off) when it is unset or the exporter
/// cannot be built; the latter warns once on stderr and the service keeps
/// running with local logging only. Must run inside a Tokio runtime.
pub fn init_provider_from_env() -> Option<TracerProvider> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let exporter = match build_span_exporter(&endpoint) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("OTel exporter init failed ({err}); falling back to local logging only");
            return None;
        }
    };
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, Tokio)
        .with_resource(otel_resource(&service_name()))
        .build();
    OTEL_ACTIVE.store(true, Ordering::Relaxed);
    Some(provider)
}

/// `OTEL_SERVICE_NAME`, or [`SERVICE_NAME_DEFAULT`].
pub fn service_name() -> String {
    std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| SERVICE_NAME_DEFAULT.to_string())
}

/// `true` for metadata Resql itself emits (`resql` or `resql::…` targets).
fn is_own_target(target: &str) -> bool {
    target == "resql" || target.starts_with("resql::")
}

/// The `tracing` layer that exports spans through `provider`.
///
/// Only Resql's own spans pass. Events are filtered out so log lines (and
/// driver events that may carry SQL text) never become span events, and
/// dependency spans (`hyper`, `h2`, the exporter's own transport at
/// `debug`/`trace`) are not exported.
pub fn layer<S>(provider: &TracerProvider) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_opentelemetry::layer()
        .with_tracer(provider.tracer(env!("CARGO_PKG_NAME")))
        .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            meta.is_span() && is_own_target(meta.target())
        }))
}

/// Flush pending spans and stop the exporter.
pub fn shutdown(provider: Option<TracerProvider>) {
    if let Some(p) = provider {
        let _ = p.shutdown();
    }
}

/// Make `span` a child of the remote context carried by a W3C
/// `traceparent` value. Invalid values leave the span as a new root.
pub fn set_parent_from_traceparent(span: &tracing::Span, traceparent: &str) {
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent.to_string());
    let cx = TraceContextPropagator::new().extract(&carrier);
    if cx.span().span_context().is_valid() {
        span.set_parent(cx);
    }
}

/// The `traceparent` of `span` itself (`00-<trace>-<span>-<flags>`), or
/// `None` when no OTel layer is installed for it.
pub fn traceparent_of(span: &tracing::Span) -> Option<String> {
    let cx = span.context();
    let sc = cx.span().span_context().clone();
    if !sc.is_valid() {
        return None;
    }
    Some(format!(
        "00-{}-{}-{:02x}",
        sc.trace_id(),
        sc.span_id(),
        sc.trace_flags().to_u8()
    ))
}

/// `db.system` value for a pool. MariaDB shares the `mysql` value because
/// the pool does not distinguish the two.
pub fn db_system(pool: &Pool) -> &'static str {
    match pool {
        Pool::Postgres(_) => "postgresql",
        Pool::Sqlite(_) => "sqlite",
        Pool::MySql(_) => "mysql",
    }
}

/// Leading SQL keyword of a saved query (`SELECT`, `INSERT`, `UPDATE`,
/// `DELETE`, `MERGE`, `CALL`), skipping whitespace and comments. Anything
/// else (including `WITH …`) returns `None`: the operation is not known
/// without parsing the statement.
pub fn leading_sql_keyword(sql: &str) -> Option<&'static str> {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map(|(_, tail)| tail).unwrap_or("");
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map(|(_, tail)| tail).unwrap_or("");
        } else {
            break;
        }
    }
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "CALL"]
        .into_iter()
        .find(|k| *k == word)
}

/// CLIENT span for one saved-query execution, a child of the current span.
///
/// Returns `Span::none()` when export is off, so no span exists and log
/// output is unchanged. The span is not entered by the caller: log events
/// emitted during execution keep the request span as their context.
///
/// Attributes: `db.system` / `db.system.name`, `db.namespace` (datasource
/// name), `db.operation.name` (when [`leading_sql_keyword`] knows it),
/// `resql.query` (`<project>/<saved-query>`), and later `resql.rows`,
/// `db.operation.batch.size`, `error.type`. SQL text and bind parameters
/// are never recorded.
pub fn db_span(pool: &Pool, datasource: &str, query: &str, sql: &str) -> tracing::Span {
    if !otel_active() {
        return tracing::Span::none();
    }
    let system = db_system(pool);
    let operation = leading_sql_keyword(sql);
    let name = match operation {
        Some(op) => format!("{op} {query}"),
        None => query.to_string(),
    };
    let span = tracing::info_span!(
        "db_query",
        otel.name = %name,
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        // `db.system` for backends on the pre-1.33 conventions,
        // `db.system.name` for the stable ones.
        db.system = system,
        db.system.name = system,
        db.namespace = %datasource,
        db.operation.name = tracing::field::Empty,
        db.operation.batch.size = tracing::field::Empty,
        resql.query = %query,
        resql.rows = tracing::field::Empty,
        "error.type" = tracing::field::Empty,
    );
    if let Some(op) = operation {
        span.record("db.operation.name", op);
    }
    span
}

/// Record the outcome of a saved-query execution on a [`db_span`].
/// `error_code` is the `X-Resql-Error-Code` value of the failure.
pub fn record_db_outcome(span: &tracing::Span, rows: Option<usize>, error_code: Option<&str>) {
    if let Some(n) = rows {
        span.record("resql.rows", n as u64);
    }
    if let Some(code) = error_code {
        span.record("otel.status_code", "ERROR");
        span.record("error.type", code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_keyword_skips_comments_and_whitespace() {
        assert_eq!(leading_sql_keyword("SELECT 1"), Some("SELECT"));
        assert_eq!(leading_sql_keyword("  \n\tselect 1"), Some("SELECT"));
        assert_eq!(
            leading_sql_keyword("-- @transactional\nINSERT INTO t VALUES (1)"),
            Some("INSERT")
        );
        assert_eq!(
            leading_sql_keyword("/*\nparams: {}\n*/\nUPDATE t SET a = 1"),
            Some("UPDATE")
        );
        assert_eq!(leading_sql_keyword("delete from t"), Some("DELETE"));
    }

    #[test]
    fn own_target_matches_resql_modules_only() {
        assert!(is_own_target("resql"));
        assert!(is_own_target("resql::server"));
        assert!(!is_own_target("h2::proto::connection"));
        assert!(!is_own_target("resql_extra"));
    }

    #[test]
    fn leading_keyword_is_none_when_not_cheaply_known() {
        assert_eq!(
            leading_sql_keyword("WITH x AS (SELECT 1) SELECT * FROM x"),
            None
        );
        assert_eq!(leading_sql_keyword(""), None);
        assert_eq!(leading_sql_keyword("-- only a comment"), None);
        assert_eq!(leading_sql_keyword("/* unterminated"), None);
        assert_eq!(leading_sql_keyword("SELECTX 1"), None);
    }
}
