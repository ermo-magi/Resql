//! Process-level OTLP export tests: the real `resql` binary against real
//! TCP collectors (a recording sink, a listener that never answers, a
//! closed port). They cover what in-process tests cannot: the logging
//! filter wiring in `main`, boot output, and SIGTERM-to-exit time.
//!
//! Every child gets its own port and TempDir, and the parent environment's
//! `OTEL_*` / `RESQL_LOG*` variables are scrubbed, so the tests are
//! independent and safe to run concurrently.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN: &str = "S3CRET-EXPORTER-TOKEN";

/// Poll `cond` every 25 ms until it holds or `limit` passes.
fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    cond()
}

/// One recorded collector request: `(request line, authorization header)`.
type Hit = (String, String);

/// OTLP/HTTP collector that records requests and answers with a fixed status.
struct Sink {
    port: u16,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl Sink {
    fn start() -> Self {
        Self::start_answering(200)
    }

    fn start_answering(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let recorded = hits.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let recorded = recorded.clone();
                thread::spawn(move || serve_sink_connection(stream, &recorded, status));
            }
        });
        Sink { port, hits }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn trace_posts(&self) -> Vec<Hit> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _)| line.starts_with("POST /v1/traces "))
            .cloned()
            .collect()
    }
}

fn serve_sink_connection(stream: TcpStream, hits: &Mutex<Vec<Hit>>, status: u16) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let (mut length, mut auth) = (0usize, String::new());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap_or(0),
                    "authorization" => auth = value.trim().to_string(),
                    _ => {}
                }
            }
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        hits.lock()
            .unwrap()
            .push((request_line.trim_end().to_string(), auth));
        if write!(writer, "HTTP/1.1 {status} X\r\ncontent-length: 0\r\n\r\n").is_err() {
            return;
        }
    }
}

/// Accepts connections and never answers: a hung collector.
struct Stall {
    port: u16,
}

impl Stall {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        Stall { port }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A running `resql serve` child with captured stdout + stderr.
struct Resql {
    child: Child,
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
    _tmp: tempfile::TempDir,
}

impl Resql {
    fn spawn(env: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let sql = tmp.path().join("sql/demo/GET");
        std::fs::create_dir_all(&sql).unwrap();
        std::fs::write(
            sql.join("ping.sql"),
            "/*\nparams: {}\n*/\nSELECT 'pong' AS pong",
        )
        .unwrap();
        let port = free_port();
        let cfg = tmp.path().join("resql.yaml");
        std::fs::write(
            &cfg,
            format!(
                "server:\n  bind: \"127.0.0.1:{port}\"\nsql_dir: \"{}\"\n\
                 datasources:\n  - name: demo\n    url: \"sqlite::memory:\"\n",
                tmp.path().join("sql").display()
            ),
        )
        .unwrap();

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_resql"));
        cmd.arg("-c")
            .arg(&cfg)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars() {
            if key.starts_with("OTEL_") || key.starts_with("RESQL_LOG") {
                cmd.env_remove(key);
            }
        }
        cmd.env("RESQL_LOG_FORMAT", "text");
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("spawn resql");

        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        for pipe in [
            Box::new(stdout) as Box<dyn Read + Send>,
            Box::new(stderr) as Box<dyn Read + Send>,
        ] {
            let log = log.clone();
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    log.lock().unwrap().push(line);
                }
            });
        }

        let mut me = Resql {
            child,
            port,
            log,
            _tmp: tmp,
        };
        let ready = wait_until(Duration::from_secs(30), || {
            assert!(
                me.child.try_wait().unwrap().is_none(),
                "resql exited during boot:\n{}",
                me.log_text()
            );
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(ready, "resql never listened:\n{}", me.log_text());
        me
    }

    fn log_text(&self) -> String {
        self.log.lock().unwrap().join("\n")
    }

    /// `GET path`, returns the status code.
    fn get(&self, path: &str) -> u16 {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(s, "GET {path} HTTP/1.0\r\nHost: t\r\n\r\n").unwrap();
        let mut raw = String::new();
        let _ = s.read_to_string(&mut raw);
        raw.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {raw:?}"))
    }

    /// SIGTERM, then wait up to `limit`. `None` if it was still running
    /// (it is then killed so the test does not leak the process).
    fn terminate(&mut self, limit: Duration) -> Option<(ExitStatus, Duration)> {
        let start = Instant::now();
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success(), "kill -TERM failed");
        let mut status = None;
        wait_until(limit, || {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        match status {
            Some(s) => {
                // Let the reader threads drain the pipes.
                thread::sleep(Duration::from_millis(100));
                Some((s, start.elapsed()))
            }
            None => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                None
            }
        }
    }
}

impl Drop for Resql {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn http_env<'a>(url: &'a str, level: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("OTEL_EXPORTER_OTLP_ENDPOINT", url),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
        (
            "OTEL_EXPORTER_OTLP_HEADERS",
            "authorization=Bearer%20S3CRET-EXPORTER-TOKEN",
        ),
        ("RESQL_LOG", level),
    ]
}

#[test]
fn spans_reach_the_collector_at_every_log_level() {
    // The log filter decides what is LOGGED; it must never decide what is
    // EXPORTED. At `warn`/`error`/`off` the info-level request spans are
    // not logged but must still be exported, and the access-log line must
    // still obey the filter.
    for level in ["info", "warn", "error", "off"] {
        let sink = Sink::start();
        let url = sink.url();
        let mut resql = Resql::spawn(&http_env(&url, level));
        assert_eq!(resql.get("/demo/ping"), 200, "level {level}");
        let (status, _) = resql
            .terminate(Duration::from_secs(15))
            .unwrap_or_else(|| panic!("level {level}: did not exit\n{}", resql.log_text()));
        assert!(status.success(), "level {level}: {status}");

        let posts = sink.trace_posts();
        assert!(
            !posts.is_empty(),
            "level {level}: no span export reached the collector\n{}",
            resql.log_text()
        );
        assert!(
            posts
                .iter()
                .all(|(_, auth)| auth == &format!("Bearer {TOKEN}")),
            "level {level}: exporter headers not applied: {posts:?}"
        );
        let log = resql.log_text();
        assert_eq!(
            log.contains("http request completed"),
            level == "info",
            "level {level}: access log must follow the filter:\n{log}"
        );
        assert!(!log.contains(TOKEN), "level {level}: token leaked:\n{log}");
        assert!(
            !log.contains("opentelemetry"),
            "level {level}: SDK noise on the happy path:\n{log}"
        );
    }
}

#[test]
fn boot_log_announces_export_without_leaking_headers() {
    let sink = Sink::start();
    let url = format!("{}/otlp", sink.url());
    let mut resql = Resql::spawn(&http_env(&url, "info"));
    let _ = resql.terminate(Duration::from_secs(15));
    let log = resql.log_text();
    let line = log
        .lines()
        .find(|l| l.contains("OTLP trace export enabled"))
        .unwrap_or_else(|| panic!("no boot line announcing export:\n{log}"));
    // Scheme, host and port only: the path is not echoed.
    assert!(
        line.contains(&format!("endpoint=http://127.0.0.1:{}", sink.port)),
        "{line}"
    );
    assert!(!line.contains("/otlp"), "{line}");
    assert!(line.contains("http/protobuf"), "{line}");
    assert!(line.contains("service.name=resql"), "{line}");
    assert!(line.contains("export_timeout_ms=10000"), "{line}");
    assert!(!log.contains(TOKEN), "{log}");
}

#[test]
fn endpoint_with_credentials_is_refused_and_never_logged() {
    // userinfo and a query string would be echoed by exporter errors, so
    // the endpoint is refused: no export, a clear reason, service still up.
    let sink = Sink::start();
    let url = format!("http://exporter:HUNTER2@127.0.0.1:{}/?k=QSECRET", sink.port);
    let mut resql = Resql::spawn(&http_env(&url, "info"));
    assert_eq!(resql.get("/demo/ping"), 200);
    let (status, _) = resql.terminate(Duration::from_secs(15)).expect("exit");
    assert!(status.success(), "{status}");
    let log = resql.log_text();
    assert!(log.contains("OTLP trace export disabled"), "{log}");
    assert!(log.contains("must not carry credentials"), "{log}");
    assert!(!log.contains("OTLP trace export enabled"), "{log}");
    for secret in ["HUNTER2", "QSECRET", TOKEN] {
        assert!(!log.contains(secret), "{secret} leaked:\n{log}");
    }
    assert!(
        sink.hits.lock().unwrap().is_empty(),
        "nothing may be exported"
    );
}

#[test]
fn failed_exports_are_logged_and_leak_nothing() {
    // The SDK reports export failures only through its internal logs; with
    // them off a wrong endpoint or token was invisible for the process's
    // whole life. Header values must never appear in that output.
    let rejecting = Sink::start_answering(401);
    let closed = format!("http://127.0.0.1:{}", free_port());
    for (name, url, expect) in [
        ("rejected", rejecting.url(), "401"),
        ("refused", closed, "ConnectionRefused"),
    ] {
        // A short batch delay makes the periodic export, not just the
        // shutdown flush, hit the bad collector before SIGTERM.
        let mut env = http_env(&url, "info");
        env.push(("OTEL_BSP_SCHEDULE_DELAY", "100"));
        let mut resql = Resql::spawn(&env);
        assert_eq!(resql.get("/demo/ping"), 200);
        let seen = wait_until(Duration::from_secs(15), || {
            resql.log_text().lines().any(|l| l.contains("ExportError"))
        });
        assert!(
            seen,
            "{name}: export failure never logged:\n{}",
            resql.log_text()
        );
        let (status, _) = resql.terminate(Duration::from_secs(15)).expect("exit");
        assert!(status.success(), "{name}: {status}");
        let log = resql.log_text();
        assert!(
            log.lines()
                .any(|l| l.contains("ERROR") && l.contains("ExportError") && l.contains(expect)),
            "{name}: export failure not logged with its cause:\n{log}"
        );
        assert!(!log.contains(TOKEN), "{name}: header value leaked:\n{log}");
    }
}

#[test]
fn export_off_logs_no_otel_line_and_contacts_nobody() {
    let sink = Sink::start();
    let mut resql = Resql::spawn(&[("RESQL_LOG", "info")]);
    assert_eq!(resql.get("/demo/ping"), 200);
    let (status, _) = resql.terminate(Duration::from_secs(15)).expect("exit");
    assert!(status.success());
    let log = resql.log_text();
    assert!(!log.to_ascii_lowercase().contains("otlp"), "{log}");
    assert!(sink.hits.lock().unwrap().is_empty());
}

#[test]
fn shutdown_is_bounded_when_the_collector_hangs() {
    // A collector that accepts the connection and never answers used to
    // hold the process for the exporter's 30 s batch timeout, past the
    // 10 s `docker stop` grace period.
    let stall = Stall::start();
    let url = format!("http://127.0.0.1:{}", stall.port);
    let mut resql = Resql::spawn(&http_env(&url, "info"));
    for _ in 0..3 {
        assert_eq!(resql.get("/demo/ping"), 200);
    }
    let (status, took) = resql
        .terminate(Duration::from_secs(20))
        .unwrap_or_else(|| panic!("still running 20 s after SIGTERM\n{}", resql.log_text()));
    assert!(status.success(), "{status}");
    assert!(
        took < Duration::from_secs(9),
        "SIGTERM-to-exit took {took:?}, over the 10 s container grace period\n{}",
        resql.log_text()
    );
    assert!(
        resql.log_text().contains("did not finish"),
        "the abandoned flush must be logged:\n{}",
        resql.log_text()
    );
}

#[test]
fn unreachable_collector_does_not_delay_shutdown() {
    let url = format!("http://127.0.0.1:{}", free_port());
    let mut resql = Resql::spawn(&http_env(&url, "info"));
    assert_eq!(resql.get("/demo/ping"), 200);
    let (status, took) = resql.terminate(Duration::from_secs(20)).expect("exit");
    assert!(status.success(), "{status}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
    let log = resql.log_text();
    let line = log
        .lines()
        .find(|l| l.contains("OTLP span flush failed"))
        .unwrap_or_else(|| panic!("abandoned flush must be explained:\n{log}"));
    assert!(line.contains("reason="), "{line}");
}

#[test]
fn plaintext_headers_to_a_remote_collector_warn_at_boot() {
    // 192.0.2.0/24 is TEST-NET-1: never routable, nothing is contacted
    // because no request is served before SIGTERM.
    let mut resql = Resql::spawn(&http_env("http://192.0.2.1:4318", "info"));
    let _ = resql.terminate(Duration::from_secs(20)).expect("exit");
    let log = resql.log_text();
    let warn = log
        .lines()
        .find(|l| l.contains("WARN") && l.contains("plaintext"))
        .unwrap_or_else(|| panic!("no plaintext warning:\n{log}"));
    assert!(!warn.contains(TOKEN), "{warn}");
}

#[test]
fn https_endpoint_with_grpc_warns_at_boot() {
    let mut resql = Resql::spawn(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://127.0.0.1:1"),
        ("RESQL_LOG", "info"),
    ]);
    let _ = resql.terminate(Duration::from_secs(20)).expect("exit");
    let log = resql.log_text();
    assert!(
        log.lines()
            .any(|l| l.contains("WARN") && l.contains("gRPC") && l.contains("http/protobuf")),
        "no https+gRPC warning:\n{log}"
    );
}

#[test]
fn loopback_plaintext_collector_with_headers_does_not_warn() {
    let sink = Sink::start();
    let url = sink.url();
    let mut resql = Resql::spawn(&http_env(&url, "info"));
    let _ = resql.terminate(Duration::from_secs(20)).expect("exit");
    let log = resql.log_text();
    assert!(
        !log.lines().any(|l| l.contains("WARN")),
        "loopback collector must not warn:\n{log}"
    );
}
