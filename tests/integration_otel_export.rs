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
use opentelemetry::trace::{SpanId, SpanKind, Status};
use opentelemetry_sdk::trace::TracerProvider;

#[path = "common/otel_capture.rs"]
mod otel_capture;
use otel_capture::{attr, capture_with, Captured};

fn otel_capture() -> (Captured, TracerProvider, tracing::dispatcher::DefaultGuard) {
    let (captured, provider, _, guard) = capture_with("info", "text");
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
    captured.wait_for_servers(1);
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
    // The span id recorded for JSON logs is the exported span's own.
    assert_eq!(attr(server, "span_id"), Some(ctx.span_id().to_string()));
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
    captured.wait_for_servers(2);

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
    captured.wait_for_servers(1);
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
    captured.wait_for_servers(1);
    assert_eq!(status, 200);

    let db = &captured.of_kind(SpanKind::Client)[0];
    assert_eq!(db.name, "SELECT demo/lookup");
    assert_eq!(attr(db, "db.operation.batch.size").as_deref(), Some("3"));
    assert_eq!(attr(db, "resql.rows").as_deref(), Some("2"));
}

#[tokio::test]
async fn request_span_route_is_a_template_never_the_raw_path() {
    let app = TestAppBuilder::new()
        .with_sql("demo/GET/ping.sql", PING_SQL)
        .with_sql("demo/POST/add.sql", "SELECT :v AS v")
        .build()
        .await;
    let (captured, provider, guard) = otel_capture();
    let (s1, _) = app.request("GET", "/demo/RAW-PATH-1", None, &[]).await;
    let (s2, _) = app.request("GET", "/RAW-PATH-2", None, &[]).await;
    let (s3, _) = app
        .request(
            "POST",
            "/demo/add/batch",
            Some(r#"{"queries":[{"v":"a"}]}"#),
            &[],
        )
        .await;
    drop(guard);
    let _ = provider.force_flush();
    captured.wait_for_servers(3);
    assert_eq!((s1, s2, s3), (400, 404, 200));

    let servers = captured.of_kind(SpanKind::Server);
    let names: Vec<_> = servers.iter().map(|s| s.name.to_string()).collect();
    assert_eq!(
        names,
        vec![
            "GET /:project/*tail",
            "GET unmatched",
            "POST /demo/add/batch"
        ]
    );
    for span in &servers {
        let dump = format!("{} {:?}", span.name, span.attributes);
        assert!(!dump.contains("RAW-PATH"), "raw path leaked: {dump}");
    }
    assert_eq!(attr(&servers[2], "resql.project").as_deref(), Some("demo"));
    assert_eq!(
        attr(&servers[2], "http.route").as_deref(),
        Some("/demo/add/batch")
    );
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
    captured.wait_for_servers(1);
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

#[tokio::test]
async fn dependency_spans_are_not_exported() {
    let (captured, provider, guard) = otel_capture();
    tracing::info_span!(target: "h2::proto", "dependency_span").in_scope(|| {});
    tracing::info_span!(target: "resql::server", "own_span").in_scope(|| {});
    drop(guard);
    let _ = provider.force_flush();
    let names: Vec<_> = captured.all().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["own_span"]);
}

#[tokio::test]
async fn json_log_lines_carry_the_exported_span_and_trace_ids() {
    let app = TestAppBuilder::new()
        .with_sql("demo/GET/ping.sql", PING_SQL)
        .build()
        .await;
    let (captured, provider, logs, guard) = capture_with("info", "json");
    app.request_full("GET", "/demo/ping", None, &[("traceparent", INBOUND)])
        .await;
    drop(guard);
    let _ = provider.force_flush();
    captured.wait_for_servers(1);

    let server = &captured.of_kind(SpanKind::Server)[0];
    let line = logs
        .text()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["fields"]["message"] == "http request completed")
        .expect("access log line");
    assert_eq!(
        line["span"]["span_id"],
        server.span_context.span_id().to_string()
    );
    assert_eq!(line["span"]["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
}
