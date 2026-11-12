//! Exporter behaviour against misbehaving collectors, in process: the
//! per-export timeout and the shutdown deadline. The binary-level versions
//! live in `integration_otel_process.rs`; these pin the library contract
//! and need no environment variables, so they run in parallel safely.

use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::TracerProvider;
use resql::observability::{
    build_span_exporter_with, shutdown_within, OtlpTransport, SHUTDOWN_DEADLINE,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

/// Accepts connections and never answers. Returns the endpoint URL.
async fn silent_collector() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    url
}

/// OTLP/HTTP collector that records `METHOD path` per request and answers 200.
async fn recording_collector() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let recorded = recorded.clone();
        async move {
            recorded
                .lock()
                .unwrap()
                .push(format!("{} {}", req.method(), req.uri().path()));
            axum::http::StatusCode::OK
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// Exactly one export request arrived. (The path is the library's concern:
/// with the endpoint given programmatically it is posted to as-is; the
/// `/v1/traces` suffix of the env-var path is covered in
/// `integration_otel_export.rs`.)
fn assert_one_export(seen: &Mutex<Vec<String>>) {
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].starts_with("POST "), "{seen:?}");
}

fn provider_for(url: &str, transport: OtlpTransport, timeout: Duration) -> TracerProvider {
    let exporter = build_span_exporter_with(url, transport, timeout).expect("exporter");
    TracerProvider::builder()
        .with_batch_exporter(exporter, Tokio)
        .build()
}

fn emit_span(provider: &TracerProvider) {
    let mut span = provider.tracer("t").start("probe");
    span.end();
}

/// `force_flush` blocks, so it runs off the runtime workers.
async fn flush_blocking(provider: &TracerProvider) -> (Duration, Vec<bool>) {
    let p = provider.clone();
    let started = Instant::now();
    let results = tokio::task::spawn_blocking(move || p.force_flush())
        .await
        .unwrap();
    (
        started.elapsed(),
        results.iter().map(|r| r.is_ok()).collect(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_export_is_cut_off_by_the_configured_timeout() {
    // Before: the HTTP client had no timeout and OTEL_EXPORTER_OTLP_TIMEOUT
    // was ignored, so a flush against a hung collector waited for the batch
    // processor's own 30 s cap.
    let url = silent_collector().await;
    let provider = provider_for(
        &url,
        OtlpTransport::HttpProtobuf,
        Duration::from_millis(300),
    );
    emit_span(&provider);
    let (took, ok) = flush_blocking(&provider).await;
    assert!(took < Duration::from_secs(5), "flush took {took:?}");
    assert!(
        ok.iter().any(|ok| !ok),
        "the timed-out export must be reported: {ok:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_export_succeeds_well_inside_the_timeout() {
    let (url, seen) = recording_collector().await;
    let provider = provider_for(
        &url,
        OtlpTransport::HttpProtobuf,
        Duration::from_millis(300),
    );
    emit_span(&provider);
    let (_, ok) = flush_blocking(&provider).await;
    assert!(ok.iter().all(|ok| *ok), "{ok:?}");
    assert_one_export(&seen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_returns_at_the_deadline_when_the_collector_hangs() {
    // The client timeout is deliberately far longer than the deadline, so
    // only the deadline can end the wait.
    let url = silent_collector().await;
    let provider = provider_for(&url, OtlpTransport::HttpProtobuf, Duration::from_secs(60));
    emit_span(&provider);
    let started = Instant::now();
    let finished = shutdown_within(Some(provider), Duration::from_millis(400)).await;
    let took = started.elapsed();
    assert!(!finished, "must report the abandoned flush");
    assert!(
        took >= Duration::from_millis(400) && took < Duration::from_secs(3),
        "took {took:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_flushes_queued_spans_when_the_collector_is_healthy() {
    let (url, seen) = recording_collector().await;
    let provider = provider_for(&url, OtlpTransport::HttpProtobuf, Duration::from_secs(5));
    emit_span(&provider);
    assert!(shutdown_within(Some(provider), Duration::from_secs(5)).await);
    assert_one_export(&seen);
}

#[tokio::test]
async fn shutdown_works_on_a_single_threaded_runtime_and_without_a_provider() {
    // `#[tokio::test]` is current_thread: the flush thread must not need a
    // second runtime worker, and the waiting future must not block the one
    // thread the batch task also needs.
    assert!(shutdown_within(None, Duration::from_millis(10)).await);
    let (url, seen) = recording_collector().await;
    let provider = provider_for(&url, OtlpTransport::HttpProtobuf, Duration::from_secs(5));
    emit_span(&provider);
    assert!(shutdown_within(Some(provider), Duration::from_secs(5)).await);
    assert_one_export(&seen);
}

#[tokio::test]
async fn shutdown_with_an_idle_provider_is_immediate() {
    let (url, _) = recording_collector().await;
    let provider = provider_for(&url, OtlpTransport::HttpProtobuf, Duration::from_secs(5));
    let started = Instant::now();
    assert!(shutdown_within(Some(provider), SHUTDOWN_DEADLINE).await);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_export_to_a_silent_peer_is_bounded_by_the_shutdown_deadline() {
    // tonic gets the same timeout, but a peer that completes the TCP
    // handshake and never speaks HTTP/2 is only guaranteed to be bounded by
    // the shutdown deadline: that is the contract asserted here.
    let url = silent_collector().await;
    let provider = provider_for(&url, OtlpTransport::Grpc, Duration::from_millis(300));
    emit_span(&provider);
    let started = Instant::now();
    let _ = shutdown_within(Some(provider), Duration::from_secs(2)).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}
