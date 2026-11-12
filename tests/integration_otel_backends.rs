//! Query spans against real Postgres and MySQL pools. The SQLite path is
//! covered in `integration_otel_export.rs`; these pin the per-pool
//! `db.system` value and that a request on a non-SQLite pool still yields
//! exactly one SERVER and one CLIENT span in the caller's trace.
//!
//! Skipped (with a note on stderr) unless `TEST_POSTGRES_URL` /
//! `TEST_MYSQL_URL` is set, like `integration_postgres.rs` and
//! `integration_mysql.rs`. CI sets both.

mod common;
#[path = "common/otel_capture.rs"]
mod otel_capture;

use common::TestAppBuilder;
use opentelemetry::trace::SpanKind;
use otel_capture::{attr, capture_with};

const INBOUND: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const ONE_SQL: &str = "/*\nparams: {}\n*/\nSELECT 1 AS one";

async fn assert_query_span(app: common::TestApp, project: &str, system: &str) {
    let (captured, provider, _logs, guard) = capture_with("info", "text");
    let (status, _, _) = app
        .request_full(
            "GET",
            &format!("/{project}/one"),
            None,
            &[("traceparent", INBOUND)],
        )
        .await;
    drop(guard);
    let _ = provider.force_flush();
    captured.wait_for_servers(1);
    assert_eq!(status, 200, "{system}");

    let servers = captured.of_kind(SpanKind::Server);
    assert_eq!(servers.len(), 1, "{system}: {servers:?}");
    let server = &servers[0];
    assert_eq!(server.name, format!("GET /{project}/one"));
    assert_eq!(
        server.span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );

    let clients = captured.of_kind(SpanKind::Client);
    assert_eq!(clients.len(), 1, "{system}: {clients:?}");
    let db = &clients[0];
    assert_eq!(db.parent_span_id, server.span_context.span_id(), "{system}");
    assert_eq!(db.name, format!("SELECT {project}/one"));
    assert_eq!(attr(db, "db.system").as_deref(), Some(system));
    assert_eq!(attr(db, "db.system.name").as_deref(), Some(system));
    assert_eq!(attr(db, "db.namespace").as_deref(), Some(project));
    assert_eq!(attr(db, "resql.rows").as_deref(), Some("1"));
    let dump = format!("{:?}", db.attributes);
    assert!(!dump.contains("SELECT 1"), "SQL text leaked: {dump}");
}

#[tokio::test]
async fn postgres_query_span_reports_postgresql() {
    let Ok(url) = std::env::var("TEST_POSTGRES_URL") else {
        eprintln!("SKIP: TEST_POSTGRES_URL is unset");
        return;
    };
    let app = TestAppBuilder::new()
        .no_sqlite_datasources()
        .with_postgres_datasource("pg", &url)
        .with_sql("pg/GET/one.sql", ONE_SQL)
        .build()
        .await;
    assert_query_span(app, "pg", "postgresql").await;
}

#[tokio::test]
async fn mysql_query_span_reports_mysql() {
    let Ok(url) = std::env::var("TEST_MYSQL_URL") else {
        eprintln!("SKIP: TEST_MYSQL_URL is unset");
        return;
    };
    let app = TestAppBuilder::new()
        .no_sqlite_datasources()
        .with_mysql_datasource("my", &url)
        .with_sql("my/GET/one.sql", ONE_SQL)
        .build()
        .await;
    assert_query_span(app, "my", "mysql").await;
}
