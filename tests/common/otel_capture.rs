//! In-memory OTel capture shared by the OTLP test binaries (included with
//! `#[path]`, so binaries that do not need it do not compile it).
#![allow(dead_code)]

use futures_util::future::BoxFuture;
use opentelemetry::trace::SpanKind;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry_sdk::trace::TracerProvider;
use resql::observability;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Captured {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        self.0.lock().unwrap().extend(batch);
        Box::pin(async { Ok(()) })
    }
}

impl Captured {
    pub fn all(&self) -> Vec<SpanData> {
        self.0.lock().unwrap().clone()
    }
    pub fn of_kind(&self, kind: SpanKind) -> Vec<SpanData> {
        self.all()
            .into_iter()
            .filter(|s| s.span_kind == kind)
            .collect()
    }

    /// Wait (bounded) until `n` SERVER spans have been exported.
    ///
    /// The request span is not always closed when the response is back:
    /// sqlx-sqlite hands a clone of the current span to its worker thread
    /// with every command (`sqlx-sqlite` `connection/worker.rs`), and that
    /// thread drops it a moment later. Asserting right after the response
    /// therefore intermittently misses the last request's span. Postgres
    /// and MySQL pools have no such thread. Never waits past 5 s, and
    /// callers still assert the exact count afterwards.
    pub fn wait_for_servers(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.of_kind(SpanKind::Server).len() < n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// In-memory log sink for the `fmt` layer.
#[derive(Clone, Default)]
pub struct LogBuf(Arc<Mutex<Vec<u8>>>);

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
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Thread-local subscriber built by the same `observability::build_dispatch`
/// the binary uses, exporting into `Captured`. `#[tokio::test]` runs on the
/// current thread, so the guard covers the whole request.
pub fn capture_with(
    directive: &str,
    format: &str,
) -> (
    Captured,
    TracerProvider,
    LogBuf,
    tracing::dispatcher::DefaultGuard,
) {
    observability::set_otel_active_for_tests(true);
    let captured = Captured::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(captured.clone())
        .build();
    let logs = LogBuf::default();
    let dispatch = observability::build_dispatch(
        EnvFilter::new(directive),
        format,
        logs.clone(),
        Some(&provider),
    );
    let guard = tracing::dispatcher::set_default(&dispatch);
    (captured, provider, logs, guard)
}
