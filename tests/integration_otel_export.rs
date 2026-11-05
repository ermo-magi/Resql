//! OTLP trace export: request (SERVER) spans, saved-query (CLIENT) spans,
//! data exclusion and exporter configuration.
//!
//! Before this change Resql had no span export: `OTEL_EXPORTER_OTLP_*`
//! was ignored and the request span existed only as log context. These
//! tests drive the router with an in-memory exporter behind the same
//! `observability::layer` the binary installs.
//!
//! With export OFF nothing changes; that path stays covered by
//! `integration_observability.rs` (traceparent adopted and echoed verbatim).
//!
//! Every test in this binary flips the process-global "export active" flag
//! on, so no test here may assert export-off behaviour.

mod common;

use common::TestAppBuilder;
use futures_util::future::BoxFuture;
use opentelemetry::trace::{SpanId, SpanKind, Status};
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry_sdk::trace::TracerProvider;
use resql::observability;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone, Default)]
struct Captured(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Captured {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        self.0.lock().unwrap().extend(batch);
        Box::pin(async { Ok(()) })
    }
}

impl Captured {
    fn all(&self) -> Vec<SpanData> {
        self.0.lock().unwrap().clone()
    }
    fn of_kind(&self, kind: SpanKind) -> Vec<SpanData> {
        self.all()
            .into_iter()
            .filter(|s| s.span_kind == kind)
            .collect()
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// Thread-local subscriber with the production OTel layer exporting into
/// `Captured`. `#[tokio::test]` runs on the current thread, so the guard
/// covers the whole request.
fn otel_capture() -> (Captured, TracerProvider, tracing::subscriber::DefaultGuard) {
    observability::set_otel_active_for_tests(true);
    let captured = Captured::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(captured.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(observability::layer(&provider));
    let guard = tracing::subscriber::set_default(subscriber);
    (captured, provider, guard)
}

const INBOUND: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const PING_SQL: &str = "/*\nparams: {}\n*/\nSELECT 'pong' AS pong";

fn header(h: &axum::http::HeaderMap, name: &str) -> String {
    h.get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn request_and_query_spans_continue_inbound_trace() {
    let app = TestAppBuilder::new()
        .with_sql("demo/GET/ping.sql", PING_SQL)
        .build()
        .await;
    let (captured, provider, guard) = otel_capture();
    let (status, headers, _) = app
        .request_full("GET", "/demo/ping", None, &[("traceparent", INBOUND)])
        .await;
    drop(guard);
    let _ = provider.force_flush();
    assert_eq!(status, 200);

    let servers = captured.of_kind(SpanKind::Server);
    assert_eq!(servers.len(), 1, "one server span: {servers:?}");
    let server = &servers[0];
    let ctx = &server.span_context;
    assert_eq!(server.name, "GET /demo/ping");
    assert_eq!(
        ctx.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(server.parent_span_id.to_string(), "00f067aa0ba902b7");
    assert_eq!(attr(server, "http.request.method").as_deref(), Some("GET"));
    assert_eq!(attr(server, "http.route").as_deref(), Some("/demo/ping"));
    assert_eq!(
        attr(server, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(server.status, Status::Unset);
    // Same trace id on the response; the propagated span id is Resql's own
    // request span so callers' and callees' spans nest correctly.
    assert_eq!(
        header(&headers, "x-trace-id"),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(
        header(&headers, "traceparent"),
        format!("00-{}-{}-01", ctx.trace_id(), ctx.span_id())
    );

    let clients = captured.of_kind(SpanKind::Client);
    assert_eq!(clients.len(), 1, "one query span: {clients:?}");
    let db = &clients[0];
    assert_eq!(db.name, "SELECT demo/ping");
    assert_eq!(db.span_context.trace_id(), ctx.trace_id());
    assert_eq!(
        db.parent_span_id,
        ctx.span_id(),
        "query span nests under request"
    );
    assert_eq!(attr(db, "db.system").as_deref(), Some("sqlite"));
    assert_eq!(attr(db, "db.system.name").as_deref(), Some("sqlite"));
    assert_eq!(attr(db, "db.namespace").as_deref(), Some("demo"));
    assert_eq!(attr(db, "db.operation.name").as_deref(), Some("SELECT"));
    assert_eq!(attr(db, "resql.query").as_deref(), Some("demo/ping"));
    assert_eq!(attr(db, "resql.rows").as_deref(), Some("1"));
    assert_eq!(db.status, Status::Unset);
}

#[tokio::test]
async fn request_without_traceparent_starts_root_matching_x_trace_id() {
    let app = TestAppBuilder::new()
        .with_sql("demo/GET/ping.sql", PING_SQL)
        .build()
        .await;
    let (captured, provider, guard) = otel_capture();
    let (_, headers, _) = app.request_full("GET", "/demo/ping", None, &[]).await;
    let (_, bad_headers, _) = app
        .request_full("GET", "/demo/ping", None, &[("traceparent", "garbage")])
        .await;
    drop(guard);
    let _ = provider.force_flush();

    let servers = captured.of_kind(SpanKind::Server);
    assert_eq!(servers.len(), 2);
    for (span, h) in servers.iter().zip([&headers, &bad_headers]) {
        let ctx = &span.span_context;
        assert!(ctx.is_valid());
        assert_eq!(span.parent_span_id, SpanId::INVALID, "root span");
        assert_eq!(header(h, "x-trace-id"), ctx.trace_id().to_string());
        assert_eq!(
            header(h, "traceparent"),
            format!("00-{}-{}-01", ctx.trace_id(), ctx.span_id())
        );
    }
}

#[tokio::test]
async fn failed_query_marks_query_span_not_server_span() {
    let app = TestAppBuilder::new()
        .with_sql(
            "demo/GET/need.sql",
            "/*\nparams:\n  login: { type: string, required: true }\n*/\nSELECT :login AS l",
        )
        .build()
        .await;
    let (captured, provider, guard) = otel_capture();
    let (status, code, _, _) = app.request_err("GET", "/demo/need", None, &[]).await;
    drop(guard);
    let _ = provider.force_flush();
    assert_eq!(status, 400);

    let server = &captured.of_kind(SpanKind::Server)[0];
    assert_eq!(
        attr(server, "http.response.status_code").as_deref(),
        Some("400")
    );
    assert_eq!(server.status, Status::Unset, "4xx is not a server error");
    let db = &captured.of_kind(SpanKind::Client)[0];
    assert!(matches!(db.status, Status::Error { .. }), "{:?}", db.status);
    assert_eq!(attr(db, "error.type"), Some(code));
    assert_eq!(attr(db, "resql.rows"), None);
}

#[tokio::test]
async fn batch_query_span_records_batch_size_and_total_rows() {
    let app = TestAppBuilder::new()
        .with_seed(
            "CREATE TABLE t (login TEXT, email TEXT); \
             INSERT INTO t VALUES ('a', 'a@x'); \
             INSERT INTO t VALUES ('b', 'b@x');",
        )
        .with_sql(
            "demo/POST/lookup.sql",
            "SELECT email FROM t WHERE login = :login",
        )
        .build()
        .await;
    let (captured, provider, guard) = otel_capture();
    let (status, _) = app
        .request(
            "POST",
            "/demo/lookup/batch",
            Some(r#"{"queries": [{"login":"a"}, {"login":"b"}, {"login":"zz"}]}"#),
            &[],
        )
        .await;
    drop(guard);
    let _ = provider.force_flush();
    assert_eq!(status, 200);

    let db = &captured.of_kind(SpanKind::Client)[0];
    assert_eq!(db.name, "SELECT demo/lookup");
    assert_eq!(attr(db, "db.operation.batch.size").as_deref(), Some("3"));
    assert_eq!(attr(db, "resql.rows").as_deref(), Some("2"));
}

#[tokio::test]
async fn health_probes_produce_no_spans() {
    let app = TestAppBuilder::new().build().await;
    let (captured, provider, guard) = otel_capture();
    for path in ["/health", "/healthz"] {
        let (status, headers, _) = app.request_full("GET", path, None, &[]).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(header(&headers, "x-trace-id").len(), 32, "{path}");
    }
    drop(guard);
    let _ = provider.force_flush();
    assert!(captured.all().is_empty(), "{:?}", captured.all());
}

#[tokio::test]
async fn spans_carry_no_sql_parameters_headers_query_strings_or_events() {
    const MARKER: &str = "PII-MARKER-38001085718";
    let sql = "SELECT email FROM t WHERE login = :login";
    let app = TestAppBuilder::new()
        .with_seed("CREATE TABLE t (login TEXT, email TEXT);")
        .with_sql("demo/POST/lookup.sql", sql)
        .build()
        .await;
    let (captured, provider, guard) = otel_capture();
    let body = format!(r#"{{"login":"{MARKER}"}}"#);
    let auth = format!("Bearer {MARKER}");
    let (status, _) = app
        .request(
            "POST",
            &format!("/demo/lookup?q={MARKER}"),
            Some(&body),
            &[("authorization", &auth), ("cookie", MARKER)],
        )
        .await;
    drop(guard);
    let _ = provider.force_flush();
    assert_eq!(status, 200);

    let spans = captured.all();
    assert_eq!(spans.len(), 2, "{spans:?}");
    for span in &spans {
        let dump = format!("{} {:?}", span.name, span.attributes);
        assert!(!dump.contains(MARKER), "marker leaked: {dump}");
        assert!(!dump.contains("FROM t"), "SQL text leaked: {dump}");
        assert!(!dump.contains('?'), "query string leaked: {dump}");
        // Log events (access log, driver events) stay out of spans.
        assert!(span.events.is_empty(), "span events: {:?}", span.events);
    }
}

/// Every env-var-driven check lives in this one test: env is process-global
/// and the other tests in this binary run in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_and_resource_follow_otel_env_vars() {
    use observability::{otlp_transport_from_env, OtlpTransport};

    for key in [
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    ] {
        std::env::remove_var(key);
    }
    assert_eq!(otlp_transport_from_env(), OtlpTransport::Grpc);
    std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf");
    assert_eq!(otlp_transport_from_env(), OtlpTransport::HttpProtobuf);
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc");
    assert_eq!(
        otlp_transport_from_env(),
        OtlpTransport::Grpc,
        "per-signal wins"
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json");
    assert_eq!(
        otlp_transport_from_env(),
        OtlpTransport::Grpc,
        "unsupported -> grpc"
    );
    std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL");

    // Service name: env wins, empty falls back to the default.
    std::env::set_var("OTEL_SERVICE_NAME", "");
    assert_eq!(observability::service_name(), "resql");
    std::env::set_var("OTEL_SERVICE_NAME", "my-sql-api");
    assert_eq!(observability::service_name(), "my-sql-api");
    std::env::remove_var("OTEL_SERVICE_NAME");

    // Resource: OTEL_RESOURCE_ATTRIBUTES applies, service.name from us wins.
    std::env::set_var(
        "OTEL_RESOURCE_ATTRIBUTES",
        "deployment.environment=staging,service.namespace=my-system,service.name=ignored",
    );
    let res = observability::otel_resource("my-sql-api");
    let get = |k: &str| {
        res.get(opentelemetry::Key::new(k.to_string()))
            .map(|v| v.to_string())
    };
    assert_eq!(get("service.name").as_deref(), Some("my-sql-api"));
    assert_eq!(get("deployment.environment").as_deref(), Some("staging"));
    assert_eq!(get("service.namespace").as_deref(), Some("my-system"));
    assert_eq!(
        get("service.version").as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    std::env::remove_var("OTEL_RESOURCE_ATTRIBUTES");

    // No endpoint (or an empty one) leaves export off.
    std::env::remove_var("OTEL_EXPORTER_OTLP_PROTOCOL");
    std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    assert!(observability::init_provider_from_env().is_none());
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "");
    assert!(observability::init_provider_from_env().is_none());

    // gRPC over TLS builds (lazy channel, nothing is dialled here).
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "https://collector.example:4317",
    );
    observability::build_span_exporter("https://collector.example:4317")
        .expect("https gRPC exporter builds");

    // http/protobuf: POST <endpoint>/v1/traces with OTEL_EXPORTER_OTLP_HEADERS.
    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;
    let seen: Seen = Arc::default();
    let sink = {
        let seen = seen.clone();
        axum::Router::new().fallback(move |req: axum::extract::Request| {
            let seen = seen.clone();
            async move {
                let h = |n: &str| {
                    req.headers()
                        .get(n)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                };
                seen.lock().unwrap().push((
                    format!("{} {}", req.method(), req.uri().path()),
                    h("authorization"),
                    h("content-type"),
                ));
                axum::http::StatusCode::OK
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, sink).await.unwrap() });

    std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf");
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &url);
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_HEADERS",
        "authorization=Bearer%20s3cret",
    );
    let exporter = observability::build_span_exporter(&url).expect("http exporter");
    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build();
    {
        use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
        let mut span = provider.tracer("t").start("probe");
        span.end();
    }
    let flushed = tokio::task::spawn_blocking(move || {
        let r = provider.force_flush();
        let _ = provider.shutdown();
        r
    })
    .await
    .unwrap();
    for r in flushed {
        r.expect("flush");
    }
    for key in [
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_HEADERS",
    ] {
        std::env::remove_var(key);
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(
            "POST /v1/traces".to_string(),
            "Bearer s3cret".to_string(),
            "application/x-protobuf".to_string()
        )]
    );
}
