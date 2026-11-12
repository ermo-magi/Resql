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
//! | `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` / `OTEL_EXPORTER_OTLP_TIMEOUT` | Per-export timeout in ms, applied to both transports (default 10 000) |
//! | `OTEL_SERVICE_NAME` | `service.name` (default `resql`) |
//! | `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes |
//!
//! Export is independent of the log filter: `RESQL_LOG` / `logging.level`
//! decide what is logged, never what is exported (see [`build_dispatch`]).
//! On SIGTERM the final flush is bounded by [`SHUTDOWN_DEADLINE`] (see
//! [`shutdown`]).
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
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// Default `service.name` when `OTEL_SERVICE_NAME` is unset.
pub const SERVICE_NAME_DEFAULT: &str = "resql";

/// Per-export timeout when neither `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` nor
/// `OTEL_EXPORTER_OTLP_TIMEOUT` is set (the OTel specification default).
pub const EXPORT_TIMEOUT_DEFAULT: Duration = Duration::from_secs(10);

/// Upper bound on the final span flush at shutdown. Kept under the 10 s
/// `docker stop` grace period so a hung collector cannot turn a clean
/// SIGTERM into a SIGKILL.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

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

impl OtlpTransport {
    /// The `OTEL_EXPORTER_OTLP_PROTOCOL` spelling of this transport.
    pub fn as_str(self) -> &'static str {
        match self {
            OtlpTransport::Grpc => "grpc",
            OtlpTransport::HttpProtobuf => "http/protobuf",
        }
    }
}

/// Parse a protocol value. Unknown values fall back to gRPC and come back
/// with a warning for the caller to log once the subscriber is installed.
fn transport_from(raw: &str) -> (OtlpTransport, Option<String>) {
    match raw.trim() {
        "" | "grpc" => (OtlpTransport::Grpc, None),
        "http/protobuf" => (OtlpTransport::HttpProtobuf, None),
        other => (
            OtlpTransport::Grpc,
            Some(format!(
                "OTEL_EXPORTER_OTLP_PROTOCOL={:?} is not supported (grpc, http/protobuf); using grpc",
                other.chars().take(64).collect::<String>()
            )),
        ),
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn protocol_env() -> String {
    env_nonempty("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .or_else(|| env_nonempty("OTEL_EXPORTER_OTLP_PROTOCOL"))
        .unwrap_or_default()
}

/// Resolve the OTLP transport from the standard env vars. The per-signal
/// variable wins. Unknown values fall back to gRPC; [`export_status_from_env`]
/// reports that as a warning.
pub fn otlp_transport_from_env() -> OtlpTransport {
    transport_from(&protocol_env()).0
}

/// Parse an export timeout in milliseconds. Unset or empty gives
/// [`EXPORT_TIMEOUT_DEFAULT`]; anything that is not a positive integer
/// also gives the default, with a warning naming the variable.
pub fn export_timeout(var: &str, raw: Option<&str>) -> (Duration, Option<String>) {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (EXPORT_TIMEOUT_DEFAULT, None);
    };
    match raw.parse::<u64>() {
        Ok(ms) if ms > 0 => (Duration::from_millis(ms), None),
        _ => (
            EXPORT_TIMEOUT_DEFAULT,
            Some(format!(
                "{var}={:?} is not a positive number of milliseconds; using {} ms",
                raw.chars().take(64).collect::<String>(),
                EXPORT_TIMEOUT_DEFAULT.as_millis()
            )),
        ),
    }
}

/// The timeout variable in effect (per-signal wins) and its raw value.
fn timeout_env() -> (&'static str, Option<String>) {
    for var in [
        "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
    ] {
        if let Some(v) = env_nonempty(var) {
            return (var, Some(v));
        }
    }
    ("OTEL_EXPORTER_OTLP_TIMEOUT", None)
}

/// Per-export timeout from the environment.
pub fn export_timeout_from_env() -> Duration {
    let (var, raw) = timeout_env();
    export_timeout(var, raw.as_deref()).0
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

/// Build the OTLP span exporter for `endpoint` with the transport and
/// timeout from the environment ([`otlp_transport_from_env`],
/// [`export_timeout_from_env`]). See [`build_span_exporter_with`].
pub fn build_span_exporter(endpoint: &str) -> Result<opentelemetry_otlp::SpanExporter, String> {
    build_span_exporter_with(
        endpoint,
        otlp_transport_from_env(),
        export_timeout_from_env(),
    )
}

/// Build the OTLP span exporter for `endpoint`.
///
/// `timeout` bounds every export request on both transports. The OTLP/HTTP
/// exporter in `opentelemetry-otlp` 0.27 keeps its configured timeout but
/// never applies it, so it is set on the `reqwest` client here; without
/// that a hung collector holds each export until the batch processor's own
/// 30 s cap.
///
/// TLS is provided by OTLP/HTTP through `reqwest` + rustls; the gRPC
/// exporter is plaintext (see [`ExportStatus::warnings`] for the https +
/// gRPC case). The endpoint and `OTEL_EXPORTER_OTLP_HEADERS` are read by
/// `opentelemetry-otlp`; header values are never logged here.
pub fn build_span_exporter_with(
    endpoint: &str,
    transport: OtlpTransport,
    timeout: Duration,
) -> Result<opentelemetry_otlp::SpanExporter, String> {
    match transport {
        OtlpTransport::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(timeout)
            .build()
            .map_err(|e| e.to_string()),
        OtlpTransport::HttpProtobuf => {
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| e.to_string())?;
            opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                .with_endpoint(endpoint)
                .with_timeout(timeout)
                .with_http_client(client)
                .build()
                .map_err(|e| e.to_string())
        }
    }
}

/// Why `endpoint` must not be used as given: it carries credentials. The
/// exporter's error text (surfaced through `internal-logs`) echoes the
/// endpoint URL verbatim, so credentials in it would reach the logs.
/// Fails closed: any `@`, `?` or `#` after the scheme counts, because the
/// userinfo of a URL whose password holds an unencoded delimiter cannot be
/// told apart from a path or query.
pub fn endpoint_credentials_problem(endpoint: &str) -> Option<&'static str> {
    let rest = endpoint.trim().split_once("://")?.1;
    rest.contains(['@', '?', '#']).then_some(
        "OTEL_EXPORTER_OTLP_ENDPOINT must not carry credentials (user:password@host, ?query or #fragment); \
         send secrets with OTEL_EXPORTER_OTLP_HEADERS",
    )
}

/// Build the tracer provider when `OTEL_EXPORTER_OTLP_ENDPOINT` is set and
/// non-empty. `Ok(None)` means export is off (variable unset or empty).
/// `Err` means the exporter could not be built (or the endpoint was
/// refused, see [`endpoint_credentials_problem`]): the service keeps
/// running with local logging only, and the caller logs the reason once
/// the subscriber is installed. Must run inside a Tokio runtime.
pub fn init_provider_from_env() -> Result<Option<TracerProvider>, String> {
    let Some(endpoint) = env_nonempty("OTEL_EXPORTER_OTLP_ENDPOINT") else {
        return Ok(None);
    };
    if let Some(problem) = endpoint_credentials_problem(&endpoint) {
        return Err(problem.to_string());
    }
    let exporter = build_span_exporter(&endpoint)?;
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, Tokio)
        .with_resource(otel_resource(&service_name()))
        .build();
    OTEL_ACTIVE.store(true, Ordering::Relaxed);
    Ok(Some(provider))
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

/// Build the global subscriber: one `fmt` layer for logs, plus the OTel
/// layer when `provider` is set.
///
/// `filter` (from `RESQL_LOG` / `logging.level`) is attached to the log
/// layer ONLY. It used to sit on the registry, where it also gated the OTel
/// layer: with `RESQL_LOG=warn` the info-level request and query spans were
/// disabled for everybody and nothing was exported, with no sign of it in
/// the logs. The OTel layer applies its own filter ([`layer`]); what is
/// logged and what is exported are now independent.
///
/// `format` is `"json"` for JSON lines, anything else for text. `writer` is
/// where log lines go (`std::io::stdout` in the binary).
pub fn build_dispatch<W>(
    filter: EnvFilter,
    format: &str,
    writer: W,
    provider: Option<&TracerProvider>,
) -> tracing::Dispatch
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let log_layer: Box<dyn Layer<Registry> + Send + Sync> = match format {
        "json" => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(writer)
            .with_filter(filter)
            .boxed(),
        _ => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_filter(filter)
            .boxed(),
    };
    tracing::Dispatch::new(
        tracing_subscriber::registry()
            .with(log_layer)
            .with(provider.map(layer)),
    )
}

/// Flush pending spans and stop the exporter, waiting at most
/// [`SHUTDOWN_DEADLINE`]. Returns `true` when the flush finished in time.
pub async fn shutdown(provider: Option<TracerProvider>) -> bool {
    shutdown_within(provider, SHUTDOWN_DEADLINE).await
}

/// [`shutdown`] with an explicit deadline.
///
/// `TracerProvider::shutdown` blocks its caller until the batch exporter
/// has finished, with no bound of its own beyond the batch processor's
/// 30 s export cap. It runs on a dedicated OS thread (not
/// `spawn_blocking`: dropping the runtime would wait for that task) and
/// this future gives up after `deadline`, so a hung collector costs at most
/// `deadline` and never blocks a runtime worker. Spans still queued at the
/// deadline are lost; that is logged, with the exporter's reason (the
/// endpoint cannot carry credentials, see [`endpoint_credentials_problem`],
/// and header values are never part of exporter errors).
pub async fn shutdown_within(provider: Option<TracerProvider>, deadline: Duration) -> bool {
    let Some(provider) = provider else {
        return true;
    };
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("otel-shutdown".into())
        .spawn(move || {
            let _ = done_tx.send(provider.shutdown().map_err(|e| e.to_string()));
        });
    if spawned.is_err() {
        tracing::warn!("could not start the OTLP flush thread; queued spans are lost");
        return false;
    }
    match tokio::time::timeout(deadline, done_rx).await {
        Ok(Ok(Ok(()))) => true,
        Ok(Ok(Err(reason))) => {
            tracing::warn!(
                reason = %truncate_reason(&reason),
                "OTLP span flush failed; queued spans may be lost (check collector reachability)"
            );
            false
        }
        Ok(Err(_)) => {
            tracing::warn!(
                "OTLP span flush thread ended without a result; queued spans may be lost"
            );
            false
        }
        Err(_) => {
            tracing::warn!(
                deadline_ms = deadline.as_millis() as u64,
                "OTLP span flush did not finish before the shutdown deadline; queued spans are lost"
            );
            false
        }
    }
}

/// Exporter error text for a log field: one line, bounded.
fn truncate_reason(reason: &str) -> String {
    reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(300)
        .collect()
}

/// `scheme://host[:port]` of an exporter endpoint, with userinfo, path,
/// query and fragment removed so credentials never reach logs. `None` when
/// the value is not `scheme://authority…`.
///
/// Fails closed: the host is whatever follows the LAST `@`, so a password
/// holding an unencoded `/`, `?` or `#` cannot leak a prefix. The price is
/// that an `@` in a path or query shows the wrong host; both are rare and
/// a wrong display is better than a leak.
pub fn redacted_endpoint(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.trim().split_once("://")?;
    let after_userinfo = rest.rsplit_once('@').map_or(rest, |(_, tail)| tail);
    let host_port = after_userinfo.split(['/', '?', '#']).next().unwrap_or("");
    if scheme.is_empty() || host_port.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme.to_ascii_lowercase(), host_port))
}

/// Host part of `scheme://host[:port]` (brackets kept for IPv6).
fn endpoint_host(redacted: &str) -> &str {
    let host_port = redacted.split_once("://").map_or(redacted, |(_, h)| h);
    if host_port.starts_with('[') {
        host_port.split_inclusive(']').next().unwrap_or(host_port)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    }
}

fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// What an operator needs to know about the export configuration, shared
/// by the boot log and `resql doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportStatus {
    /// `scheme://host[:port]`; never userinfo, path or query.
    pub endpoint: String,
    pub transport: OtlpTransport,
    pub service_name: String,
    pub timeout: Duration,
    /// Actionable problems with the configuration, one sentence each.
    /// None of them contains header values or endpoint credentials.
    pub warnings: Vec<String>,
}

/// Inputs of [`describe_export`], gathered from the environment by
/// [`export_status_from_env`].
pub struct ExportInputs<'a> {
    pub endpoint: &'a str,
    pub protocol: &'a str,
    pub has_headers: bool,
    pub timeout_var: &'a str,
    pub timeout_raw: Option<&'a str>,
    pub service_name: &'a str,
}

/// Pure description of an export configuration; see [`ExportStatus`].
pub fn describe_export(input: &ExportInputs<'_>) -> ExportStatus {
    let mut warnings = Vec::new();
    let (transport, protocol_warning) = transport_from(input.protocol);
    warnings.extend(protocol_warning);
    let (timeout, timeout_warning) = export_timeout(input.timeout_var, input.timeout_raw);
    warnings.extend(timeout_warning);

    let endpoint = redacted_endpoint(input.endpoint).unwrap_or_else(|| {
        warnings.push(
            "OTEL_EXPORTER_OTLP_ENDPOINT is not a scheme://host[:port] URL; the exporter will not work"
                .to_string(),
        );
        "<unparsable endpoint>".to_string()
    });
    let scheme = endpoint.split("://").next().unwrap_or("");
    if let Some(problem) = endpoint_credentials_problem(input.endpoint) {
        warnings.push(problem.to_string());
    }
    if scheme == "https" && transport == OtlpTransport::Grpc {
        warnings.push(
            "OTEL_EXPORTER_OTLP_ENDPOINT is https:// but the gRPC exporter has no TLS and every \
             export will fail; set OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf for a TLS endpoint"
                .to_string(),
        );
    }
    if scheme == "http" && input.has_headers && !is_loopback_host(endpoint_host(&endpoint)) {
        warnings.push(format!(
            "OTEL_EXPORTER_OTLP_HEADERS are sent in plaintext to non-loopback collector {endpoint}; \
             use an https:// endpoint with OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf, or a collector on loopback"
        ));
    }
    ExportStatus {
        endpoint,
        transport,
        service_name: input.service_name.to_string(),
        timeout,
        warnings,
    }
}

/// [`describe_export`] for the process environment; `None` when export is
/// off (`OTEL_EXPORTER_OTLP_ENDPOINT` unset or empty).
pub fn export_status_from_env() -> Option<ExportStatus> {
    let endpoint = env_nonempty("OTEL_EXPORTER_OTLP_ENDPOINT")?;
    let (timeout_var, timeout_raw) = timeout_env();
    let has_headers = env_nonempty("OTEL_EXPORTER_OTLP_TRACES_HEADERS").is_some()
        || env_nonempty("OTEL_EXPORTER_OTLP_HEADERS").is_some();
    Some(describe_export(&ExportInputs {
        endpoint: &endpoint,
        protocol: &protocol_env(),
        has_headers,
        timeout_var,
        timeout_raw: timeout_raw.as_deref(),
        service_name: &service_name(),
    }))
}

/// `err` with every occurrence of the raw `endpoint` replaced by its
/// redacted form, so an exporter build error cannot echo credentials.
pub fn scrub_endpoint(err: &str, endpoint: &str) -> String {
    let shown = redacted_endpoint(endpoint).unwrap_or_else(|| "<endpoint>".to_string());
    let mut out = err.replace(endpoint.trim(), &shown);
    // Also hide userinfo if the error printed it separately.
    if let Some((_, rest)) = endpoint.trim().split_once("://") {
        if let Some((userinfo, _)) = rest.rsplit_once('@').filter(|(u, _)| !u.is_empty()) {
            out = out.replace(userinfo, "<redacted>");
        }
    }
    out
}

/// Log the export configuration through `tracing`. Call after the
/// subscriber is installed, with the result of [`init_provider_from_env`]
/// (`enabled` is whether a provider was built, `init_error` its failure).
pub fn log_export_status(enabled: bool, init_error: Option<&str>) {
    let Some(status) = export_status_from_env() else {
        return;
    };
    if let Some(err) = init_error {
        let raw = env_nonempty("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_default();
        tracing::warn!(
            endpoint = %status.endpoint,
            "OTLP trace export disabled: {}",
            scrub_endpoint(err, &raw)
        );
    } else if enabled {
        tracing::info!(
            endpoint = %status.endpoint,
            protocol = status.transport.as_str(),
            service.name = %status.service_name,
            export_timeout_ms = status.timeout.as_millis() as u64,
            "OTLP trace export enabled"
        );
    }
    for warning in &status.warnings {
        // The init error already said it.
        if init_error != Some(warning.as_str()) {
            tracing::warn!("{warning}");
        }
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

/// The OTel span id of `span` (16 hex), or `None` when no OTel layer
/// recorded it.
pub fn span_id_of(span: &tracing::Span) -> Option<String> {
    let cx = span.context();
    let sc = cx.span().span_context().clone();
    sc.is_valid().then(|| sc.span_id().to_string())
}

tokio::task_local! {
    static RESOLVED_ROUTE: std::cell::RefCell<Option<String>>;
}

/// Run `f` with a slot that [`set_resolved_route`] can fill, and return the
/// route recorded while it ran. Used by the request middleware so the
/// request span is named after the saved query instead of the raw path.
pub async fn with_route_slot<F: std::future::Future>(f: F) -> (F::Output, Option<String>) {
    RESOLVED_ROUTE
        .scope(std::cell::RefCell::new(None), async {
            let out = f.await;
            let route = RESOLVED_ROUTE.with(|r| r.borrow_mut().take());
            (out, route)
        })
        .await
}

/// Record the route template of the current request (e.g. the saved-query
/// path `/<project>/<query>`). No-op outside [`with_route_slot`], i.e. when
/// export is off.
pub fn set_resolved_route(route: String) {
    let _ = RESOLVED_ROUTE.try_with(|r| *r.borrow_mut() = Some(route));
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

    const GRPC: OtlpTransport = OtlpTransport::Grpc;
    const HTTP: OtlpTransport = OtlpTransport::HttpProtobuf;

    fn inputs<'a>(endpoint: &'a str, protocol: &'a str, has_headers: bool) -> ExportInputs<'a> {
        ExportInputs {
            endpoint,
            protocol,
            has_headers,
            timeout_var: "OTEL_EXPORTER_OTLP_TIMEOUT",
            timeout_raw: None,
            service_name: "resql",
        }
    }

    #[test]
    fn export_timeout_accepts_positive_milliseconds() {
        let var = "OTEL_EXPORTER_OTLP_TIMEOUT";
        assert_eq!(export_timeout(var, None), (EXPORT_TIMEOUT_DEFAULT, None));
        assert_eq!(
            export_timeout(var, Some("  ")),
            (EXPORT_TIMEOUT_DEFAULT, None)
        );
        assert_eq!(
            export_timeout(var, Some(" 250 ")),
            (Duration::from_millis(250), None)
        );
        assert_eq!(
            export_timeout(var, Some("1")),
            (Duration::from_millis(1), None)
        );
    }

    #[test]
    fn export_timeout_rejects_everything_else_loudly() {
        let var = "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT";
        // 0 would fail every export; negatives, fractions, units, words and
        // u64 overflow are not "a positive number of milliseconds".
        for bad in ["0", "-5", "1.5", "10s", "abc", "99999999999999999999"] {
            let (d, warning) = export_timeout(var, Some(bad));
            assert_eq!(d, EXPORT_TIMEOUT_DEFAULT, "{bad}");
            let w = warning.unwrap_or_else(|| panic!("{bad}: no warning"));
            assert!(w.contains(var) && w.contains(bad), "{bad}: {w}");
        }
        let long = "x".repeat(10_000);
        let (_, w) = export_timeout(var, Some(&long));
        assert!(w.unwrap().len() < 300, "warning must cap echoed input");
    }

    #[test]
    fn transport_parsing_covers_known_unknown_and_hostile_values() {
        assert_eq!(transport_from(""), (GRPC, None));
        assert_eq!(transport_from(" grpc "), (GRPC, None));
        assert_eq!(transport_from("http/protobuf"), (HTTP, None));
        assert_eq!(transport_from(" http/protobuf\n"), (HTTP, None));
        for bad in ["http/json", "HTTP/PROTOBUF", "grpc,http", "http"] {
            let (t, w) = transport_from(bad);
            assert_eq!(t, GRPC, "{bad}");
            assert!(w.unwrap().contains("not supported"), "{bad}");
        }
        let (_, w) = transport_from(&"y".repeat(10_000));
        assert!(w.unwrap().len() < 300);
    }

    #[test]
    fn redacted_endpoint_keeps_only_scheme_host_port() {
        for (raw, want) in [
            ("http://collector:4317", "http://collector:4317"),
            (
                "HTTPS://Collector.Example:4318/v1/traces",
                "https://Collector.Example:4318",
            ),
            (
                "http://user:pw@127.0.0.1:4318/?k=v#f",
                "http://127.0.0.1:4318",
            ),
            ("http://user@host", "http://host"),
            ("http://[::1]:4317/x", "http://[::1]:4317"),
            ("  http://host:1  ", "http://host:1"),
            // Unencoded delimiters inside the password must not leak a prefix.
            ("http://u:pa/ss@host:1", "http://host:1"),
            ("http://u:pa?ss@host:1", "http://host:1"),
            ("http://u:pa#ss@host:1", "http://host:1"),
            ("http://u:p@ss@host:1", "http://host:1"),
        ] {
            assert_eq!(redacted_endpoint(raw).as_deref(), Some(want), "{raw}");
        }
        for bad in [
            "",
            "collector:4317",
            "://host",
            "http://",
            "http://@",
            "not a url",
        ] {
            assert_eq!(redacted_endpoint(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn loopback_detection_is_exact() {
        for yes in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.9.9.9",
            "[::1]",
            "::1",
        ] {
            assert!(is_loopback_host(yes), "{yes}");
        }
        for no in [
            "collector",
            "192.0.2.1",
            "0.0.0.0",
            "[2001:db8::1]",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "",
        ] {
            assert!(!is_loopback_host(no), "{no}");
        }
    }

    #[test]
    fn clean_configurations_have_no_warnings() {
        for (endpoint, protocol, headers) in [
            ("http://collector:4317", "", false),
            ("http://collector:4318", "http/protobuf", false),
            ("https://collector:4318", "http/protobuf", true),
            ("http://127.0.0.1:4318", "http/protobuf", true),
            ("http://localhost:4317", "grpc", true),
            ("http://[::1]:4317", "grpc", true),
        ] {
            let s = describe_export(&inputs(endpoint, protocol, headers));
            assert!(
                s.warnings.is_empty(),
                "{endpoint} {protocol}: {:?}",
                s.warnings
            );
        }
    }

    #[test]
    fn describe_export_warns_on_each_risky_shape() {
        let https_grpc = describe_export(&inputs("https://c:4317", "grpc", false));
        assert_eq!(https_grpc.warnings.len(), 1);
        assert!(https_grpc.warnings[0].contains("gRPC"));
        assert!(https_grpc.warnings[0].contains("http/protobuf"));

        let plain = describe_export(&inputs("http://192.0.2.1:4318", "http/protobuf", true));
        assert_eq!(plain.warnings.len(), 1);
        assert!(plain.warnings[0].contains("plaintext"));
        assert!(plain.warnings[0].contains("http://192.0.2.1:4318"));

        let bad_url = describe_export(&inputs("collector:4317", "", false));
        assert_eq!(bad_url.endpoint, "<unparsable endpoint>");
        assert!(bad_url.warnings[0].contains("scheme://host"));

        let bad_proto = describe_export(&inputs("http://c:4317", "carrier-pigeon", false));
        assert_eq!(bad_proto.transport, GRPC);
        assert!(bad_proto.warnings[0].contains("not supported"));

        let mut timeout = inputs("http://c:4317", "", false);
        timeout.timeout_var = "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT";
        timeout.timeout_raw = Some("soon");
        let t = describe_export(&timeout);
        assert_eq!(t.timeout, EXPORT_TIMEOUT_DEFAULT);
        assert!(t.warnings[0].contains("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT"));
    }

    #[test]
    fn plaintext_header_warning_needs_both_headers_and_a_remote_host() {
        let remote = "http://192.0.2.1:4318";
        assert!(describe_export(&inputs(remote, "http/protobuf", false))
            .warnings
            .is_empty());
        assert!(!describe_export(&inputs(remote, "grpc", true))
            .warnings
            .is_empty());
        assert!(
            describe_export(&inputs("https://192.0.2.1:4318", "http/protobuf", true))
                .warnings
                .is_empty()
        );
    }

    #[test]
    fn credentials_in_the_endpoint_are_refused_fail_closed() {
        for bad in [
            "http://user:pw@collector:4318",
            "http://user@collector:4318",
            "http://collector:4318/?token=abc",
            "http://collector:4318/v1#frag",
            // Unencoded delimiters in a password.
            "http://u:pa/ss@collector:4318",
            "http://u:pa?ss@collector:4318",
        ] {
            assert!(endpoint_credentials_problem(bad).is_some(), "{bad}");
        }
        for ok in [
            "http://collector:4318",
            "https://collector.example/otlp-prefix",
            "http://[::1]:4317",
            "collector:4317",
            "",
        ] {
            assert_eq!(endpoint_credentials_problem(ok), None, "{ok:?}");
        }
        let status = describe_export(&inputs("http://u:pw@c:4318", "http/protobuf", false));
        assert!(status
            .warnings
            .iter()
            .any(|w| w.contains("must not carry credentials")));
        assert!(!format!("{status:?}").contains("pw"));
    }

    #[test]
    fn status_never_carries_credentials() {
        let status = describe_export(&inputs(
            "http://exporter:HUNTER2@192.0.2.1:4318/?k=QSECRET",
            "http/protobuf",
            true,
        ));
        let dump = format!("{status:?}");
        for secret in ["HUNTER2", "QSECRET", "exporter:"] {
            assert!(!dump.contains(secret), "{secret} in {dump}");
        }
        assert_eq!(status.endpoint, "http://192.0.2.1:4318");
        assert_eq!(status.timeout, EXPORT_TIMEOUT_DEFAULT);
    }

    #[test]
    fn scrub_endpoint_removes_the_raw_url_and_userinfo() {
        let raw = "http://exporter:HUNTER2@host:4318/v1?k=QSECRET";
        let err = format!("invalid uri {raw}; user exporter:HUNTER2 rejected");
        let out = scrub_endpoint(&err, raw);
        assert!(!out.contains("HUNTER2"), "{out}");
        assert!(out.contains("http://host:4318"), "{out}");
        // Empty userinfo must not make the replace loop insert markers
        // between every character.
        assert_eq!(scrub_endpoint("boom", "http://@host"), "boom");
        assert_eq!(scrub_endpoint("boom", "garbage"), "boom");
    }

    #[test]
    fn transports_spell_the_standard_values() {
        assert_eq!(GRPC.as_str(), "grpc");
        assert_eq!(HTTP.as_str(), "http/protobuf");
    }

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
