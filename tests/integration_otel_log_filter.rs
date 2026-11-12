//! The log filter must never decide what is exported.
//!
//! It used to sit on the registry and gate the OTel layer too: at
//! `RESQL_LOG=warn` the info-level request and query spans were disabled
//! for everybody and nothing reached the collector.
//!
//! This test lives in its own binary on purpose. It installs dispatchers
//! with restrictive filters (`off`, `warn`, ...); `tracing` caches callsite
//! interest process-wide, so running next to tests that install `info`
//! dispatchers on other threads would make both flaky. Alone in its
//! process, it is deterministic. Keep it the only test in this file.

mod common;
#[path = "common/otel_capture.rs"]
mod otel_capture;

use common::TestAppBuilder;
use opentelemetry::trace::SpanKind;
use otel_capture::capture_with;

const INBOUND: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

#[tokio::test]
async fn export_is_independent_of_the_log_filter() {
    let app = TestAppBuilder::new()
        .with_sql(
            "demo/GET/ping.sql",
            "/*\nparams: {}\n*/\nSELECT 'pong' AS pong",
        )
        .build()
        .await;
    for format in ["text", "json"] {
        for directive in [
            "info",
            "warn",
            "error",
            "off",
            "resql=warn",
            "warn,resql=info",
            "debug",
        ] {
            let (captured, provider, logs, guard) = capture_with(directive, format);
            let (status, _, _) = app
                .request_full("GET", "/demo/ping", None, &[("traceparent", INBOUND)])
                .await;
            drop(guard);
            let _ = provider.force_flush();
            captured.wait_for_servers(1);
            let what = format!("{format}/{directive}");
            assert_eq!(status, 200, "{what}");
            assert_eq!(captured.of_kind(SpanKind::Server).len(), 1, "{what}");
            assert_eq!(captured.of_kind(SpanKind::Client).len(), 1, "{what}");
            // ...while the access log still obeys the filter.
            assert_eq!(
                logs.text().contains("http request completed"),
                matches!(directive, "info" | "debug" | "warn,resql=info"),
                "{what}:\n{}",
                logs.text()
            );
        }
    }
}
