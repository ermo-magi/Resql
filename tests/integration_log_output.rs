//! With trace export off, log output must be exactly what it was before
//! the OTel work. `observability::build_dispatch` moved the log filter from
//! the registry onto the log layer (so it can no longer gate span export);
//! this test replays the same requests through the previous wiring (kept
//! here as the oracle) and through `build_dispatch`, for every level and
//! both formats, and requires identical lines once the nondeterministic
//! values (timestamps, durations) are normalised.

mod common;

use common::TestAppBuilder;
use resql::observability;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

impl LogBuf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// The subscriber `main::init_tracing` built before `build_dispatch`:
/// the filter on the registry, one `fmt` layer.
fn previous_wiring(directive: &str, format: &str, writer: LogBuf) -> tracing::Dispatch {
    let base = tracing_subscriber::registry().with(EnvFilter::new(directive));
    match format {
        "json" => tracing::Dispatch::new(
            base.with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_writer(writer),
            ),
        ),
        _ => {
            tracing::Dispatch::new(base.with(tracing_subscriber::fmt::layer().with_writer(writer)))
        }
    }
}

const INBOUND: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// Drop what legitimately differs between two runs of the same traffic.
fn normalise(format: &str, raw: &str) -> Vec<String> {
    const VOLATILE: [&str; 3] = ["duration_ms", "elapsed", "elapsed_secs"];
    raw.lines()
        .map(|line| {
            if format == "json" {
                let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
                scrub(&mut v, &VOLATILE);
                v.as_object_mut().unwrap().remove("timestamp");
                v.to_string()
            } else {
                line.split_whitespace()
                    .skip(1) // timestamp
                    .map(|tok| match tok.split_once('=') {
                        Some((k, _)) if VOLATILE.contains(&k) => format!("{k}=_"),
                        _ => tok.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        })
        .collect()
}

fn scrub(v: &mut serde_json::Value, keys: &[&str]) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if keys.contains(&k.as_str()) {
                    *child = serde_json::Value::Null;
                } else {
                    scrub(child, keys);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|c| scrub(c, keys)),
        _ => {}
    }
}

#[tokio::test]
async fn export_off_log_output_is_identical_to_the_previous_wiring() {
    let app = TestAppBuilder::new()
        .with_sql(
            "demo/GET/ping.sql",
            "/*\nparams: {}\n*/\nSELECT 'pong' AS pong",
        )
        .with_sql(
            "demo/GET/need.sql",
            "/*\nparams:\n  login: { type: string, required: true }\n*/\nSELECT :login AS l",
        )
        .build()
        .await;
    assert!(
        !observability::otel_active(),
        "this binary must run with export off"
    );

    let mut compared = 0usize;
    for format in ["text", "json"] {
        for directive in [
            "info",
            "warn",
            "error",
            "off",
            "debug",
            "resql=debug,sqlx=off",
            "resql::server=info",
            "warn,resql=info",
        ] {
            let mut outputs = Vec::new();
            for new_wiring in [false, true] {
                let logs = LogBuf::default();
                let dispatch = if new_wiring {
                    observability::build_dispatch(
                        EnvFilter::new(directive),
                        format,
                        logs.clone(),
                        None,
                    )
                } else {
                    previous_wiring(directive, format, logs.clone())
                };
                let guard = tracing::dispatcher::set_default(&dispatch);
                let tp = [("traceparent", INBOUND)];
                app.request_full("GET", "/demo/ping", None, &tp).await;
                app.request_full("GET", "/demo/need", None, &tp).await; // 400
                app.request_full("GET", "/no/such/query", None, &tp).await; // 400
                app.request_full("GET", "/health", None, &tp).await; // no access log
                drop(guard);
                outputs.push(normalise(format, &logs.text()));
            }
            assert_eq!(
                outputs[0], outputs[1],
                "{format}/{directive}: output differs from the previous wiring"
            );
            compared += outputs[0].len();
        }
    }
    // Guard against a vacuous pass: the info/debug runs must have logged.
    assert!(compared >= 20, "only {compared} lines compared");
}

#[tokio::test]
async fn export_off_text_line_keeps_its_documented_shape() {
    // Pins the user-visible format independently of the oracle above, so a
    // change that altered both wirings in the same way is still caught.
    let app = TestAppBuilder::new()
        .with_sql(
            "demo/GET/ping.sql",
            "/*\nparams: {}\n*/\nSELECT 'pong' AS pong",
        )
        .build()
        .await;
    let logs = LogBuf::default();
    let dispatch =
        observability::build_dispatch(EnvFilter::new("info"), "text", logs.clone(), None);
    let guard = tracing::dispatcher::set_default(&dispatch);
    app.request_full("GET", "/demo/ping", None, &[("traceparent", INBOUND)])
        .await;
    drop(guard);
    let text = logs.text();
    let line = text.lines().next().expect("one access-log line");
    assert!(line.contains(" INFO "), "{line}");
    assert!(
        line.contains("trace_id=4bf92f3577b34da6a3ce929d0e0e4736"),
        "{line}"
    );
    assert!(line.contains("http request completed"), "{line}");
    assert!(
        !line.contains("span_id") && !line.contains("otel."),
        "no OTel-only fields with export off: {line}"
    );
}
