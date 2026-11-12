//! Environment-variable contract of OTLP trace export. Mutates the
//! process environment, so it is the ONLY test in this binary; see the
//! comment on the test. Do not add tests here.

use opentelemetry_sdk::trace::TracerProvider;
use resql::observability;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Every env-var-driven check lives in this one test, alone in its binary:
/// the environment is process-global, and the OTel SDK reads it whenever a
/// `TracerProvider` is built (`OTEL_TRACES_SAMPLER`, `OTEL_BSP_*`, ...), so
/// setting e.g. `OTEL_TRACES_SAMPLER=always_off` here would silently empty
/// the exports of any test running beside it.
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
    assert!(observability::init_provider_from_env().unwrap().is_none());
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "");
    assert!(observability::init_provider_from_env().unwrap().is_none());
    assert!(observability::export_status_from_env().is_none());

    // Credentials in the endpoint are refused (exporter errors echo the
    // URL), without echoing them back.
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "http://user:HUNTER2@collector:4318/?k=QSECRET",
    );
    let err = observability::init_provider_from_env().expect_err("credentials refused");
    assert!(err.contains("must not carry credentials"), "{err}");
    assert!(
        !err.contains("HUNTER2") && !err.contains("QSECRET"),
        "{err}"
    );

    // Timeout: per-signal wins, empty counts as unset, junk falls back.
    for key in [
        "OTEL_EXPORTER_OTLP_TIMEOUT",
        "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
    ] {
        std::env::remove_var(key);
    }
    assert_eq!(
        observability::export_timeout_from_env(),
        observability::EXPORT_TIMEOUT_DEFAULT
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_TIMEOUT", "1234");
    assert_eq!(
        observability::export_timeout_from_env(),
        Duration::from_millis(1234)
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "77");
    assert_eq!(
        observability::export_timeout_from_env(),
        Duration::from_millis(77)
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "");
    assert_eq!(
        observability::export_timeout_from_env(),
        Duration::from_millis(1234)
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "nope");
    assert_eq!(
        observability::export_timeout_from_env(),
        observability::EXPORT_TIMEOUT_DEFAULT
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://192.0.2.9:4318");
    let status = observability::export_status_from_env().expect("export on");
    assert!(
        status.warnings.iter().any(|w| w.contains("TRACES_TIMEOUT")),
        "{:?}",
        status.warnings
    );
    for key in [
        "OTEL_EXPORTER_OTLP_TIMEOUT",
        "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
    ] {
        std::env::remove_var(key);
    }

    // Export status: plaintext headers to a remote collector, from either
    // the shared or the per-signal variable; loopback and https are fine.
    std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf");
    assert!(observability::export_status_from_env()
        .unwrap()
        .warnings
        .is_empty());
    std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "a=b");
    assert!(observability::export_status_from_env().unwrap().warnings[0].contains("plaintext"));
    std::env::remove_var("OTEL_EXPORTER_OTLP_HEADERS");
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_HEADERS", "a=b");
    assert!(observability::export_status_from_env().unwrap().warnings[0].contains("plaintext"));
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318");
    assert!(observability::export_status_from_env()
        .unwrap()
        .warnings
        .is_empty());
    std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_HEADERS");
    // An empty per-signal protocol is "unset": the shared one applies.
    std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "");
    assert_eq!(
        observability::export_status_from_env().unwrap().transport,
        observability::OtlpTransport::HttpProtobuf
    );
    std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL");
    std::env::remove_var("OTEL_EXPORTER_OTLP_PROTOCOL");

    // Sampling is controlled with the standard OTEL_TRACES_SAMPLER
    // variables (there is no Resql-specific ratio knob).
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
    for (sampler, arg, recording) in [
        (None, None, true),
        (Some("always_on"), None, true),
        (Some("always_off"), None, false),
        (Some("traceidratio"), Some("0"), false),
        (Some("parentbased_traceidratio"), Some("0"), false),
        (Some("traceidratio"), Some("1"), true),
    ] {
        match sampler {
            Some(s) => std::env::set_var("OTEL_TRACES_SAMPLER", s),
            None => std::env::remove_var("OTEL_TRACES_SAMPLER"),
        }
        match arg {
            Some(a) => std::env::set_var("OTEL_TRACES_SAMPLER_ARG", a),
            None => std::env::remove_var("OTEL_TRACES_SAMPLER_ARG"),
        }
        let provider = observability::init_provider_from_env()
            .unwrap()
            .expect("export on");
        let span = {
            use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
            provider.tracer("t").start("probe").is_recording()
        };
        assert_eq!(span, recording, "{sampler:?} {arg:?}");
        // Nothing listens on port 1; only the sampling decision matters.
        let _ = observability::shutdown_within(Some(provider), Duration::from_secs(2)).await;
    }
    std::env::remove_var("OTEL_TRACES_SAMPLER");
    std::env::remove_var("OTEL_TRACES_SAMPLER_ARG");
    std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");

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
