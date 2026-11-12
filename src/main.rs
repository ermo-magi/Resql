use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info, warn};

use resql::config::Config;
use resql::config_compat::DiagLevel;
use resql::server;

#[derive(Debug, Parser)]
#[command(
    name = "resql",
    version,
    about = "SQL-files-as-REST-endpoints microservice"
)]
struct Cli {
    /// Path to the YAML config file. When omitted, the following candidates
    /// are tried in order (Rust-canonical first, Java-canonical second):
    /// `/app/resql.yaml`, `./resql.yaml`, `./application.yml` (Java default),
    /// `./application-prod.yml`, `./application-dev.yml`, `./application-test.yml`.
    /// The first existing candidate wins.
    #[arg(short = 'c', long = "config", env = "RESQL_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP service (default when no subcommand is given).
    Serve,
    /// Pre-boot health check — parse the config, run every safety
    /// check that `serve` runs, and exit non-zero if the deployment
    /// would fail to boot or would boot into a knowingly unsafe
    /// posture. Never binds a port, never opens a datasource pool,
    /// never mutates any external state.
    ///
    /// Exit code convention:
    ///   0 — every check passed
    ///   1 — one or more errors (config parse failure, semantic
    ///       validation, refuse-on-non-loopback-without-auth)
    ///   2 — no errors but one or more security warnings emitted;
    ///       only returned when `--strict` is passed. Without
    ///       `--strict`, warnings print but exit stays 0.
    Doctor {
        /// Elevate security warnings to failures (exit code 2).
        /// Off by default so operators can wire `doctor` into CI as a
        /// non-blocking check and still see the warning stream.
        #[arg(long)]
        strict: bool,
    },
    /// Container-native HTTP health probe. Does a raw TCP+HTTP GET
    /// against the given URL and exits 0 if the response status is
    /// 2xx, 1 otherwise. Implemented with `std::net::TcpStream` so
    /// it needs no shell + no curl — this is what lets the shipped
    /// runtime image be `distroless/static` (no libc, no libssl3,
    /// no curl → zero CVE surface).
    Health {
        /// URL to probe. Only http:// is supported.
        #[arg(long, default_value = "http://127.0.0.1:8080/health")]
        url: String,
        /// Connect + read timeout in milliseconds.
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
}

const CONFIG_CANDIDATES: &[&str] = &[
    "/app/resql.yaml",
    "./resql.yaml",
    "./application.yml",
    "./application-prod.yml",
    "./application-dev.yml",
    "./application-test.yml",
];

fn resolve_config_path(explicit: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        return Ok(explicit.clone());
    }
    for c in CONFIG_CANDIDATES {
        let p = Path::new(c);
        if p.exists() {
            return Ok(p.to_path_buf());
        }
    }
    anyhow::bail!(
        "no config file found; searched {} and none exist. Set -c/--config or RESQL_CONFIG.",
        CONFIG_CANDIDATES.join(", ")
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command.as_ref().unwrap_or(&Command::Serve) {
        Command::Serve => match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(run_serve(cli.config.as_ref()))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e:?}");
                ExitCode::FAILURE
            }
        },
        Command::Doctor { strict } => doctor(cli.config.as_ref(), *strict),
        Command::Health { url, timeout_ms } => health_check(url, *timeout_ms),
    }
}

async fn run_serve(config: Option<&PathBuf>) -> Result<()> {
    let config_path = resolve_config_path(config)?;
    let cfg = Config::from_path(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let tracer_provider = init_tracing(&cfg.logging.level, &cfg.logging.format);

    // Boot-time diagnostic pass (REFACTO-REQUIREMENTS §6.2): every Java-shape
    // field the compat shim recognised is announced here so the operator can
    // determine "am I depending on anything the target doesn't implement?"
    // from a single boot-log read.
    for d in &cfg.compat_diagnostics {
        match d.level {
            DiagLevel::Warn => warn!(field = %d.source_field, "{}", d.message),
            DiagLevel::Info => info!(field = %d.source_field, "{}", d.message),
        }
    }

    // Boot-time security-posture warnings. FN1 in the h2ck.me v1 runtime
    // pass — the shipped resql.yaml set `cors.allowed_origins: "*"` while
    // the CHANGELOG claimed CORS defaulted to closed. Now every knowingly
    // permissive knob emits one WARN so ops teams see the same audit line
    // a reviewer would.
    for w in cfg.security_warnings() {
        warn!(field = %w.field, "{}", w.message);
    }

    // Fleet stronghold §3.1: fail fast before we bind a public
    // interface with no authentication story. `validate()` handles
    // semantic checks; this method covers the deployment posture and
    // is intentionally separate so a compat-shim parse still works.
    cfg.validate_runtime_posture()
        .context("boot-time security-posture check")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        bind = %cfg.server.bind,
        sql_dir = %cfg.sql_dir.display(),
        datasources = cfg.datasources.len(),
        "starting Resql"
    );

    let state = server::init(cfg.clone())
        .await
        .context("initialising app state")?;

    info!(
        endpoints = state.index.len(),
        datasources = state.registry.names().len(),
        "app state ready"
    );

    let app = server::router(state.clone());
    let listener = TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    let addr = listener.local_addr().context("querying local addr")?;
    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;

    // T-21: after axum's graceful drain resolves, close every DB pool
    // explicitly. `PgPool::close().await` is sqlx's documented way to
    // signal + await drain — without it, dropping the Arc just runs
    // the pool's Drop impl, which does NOT wait for in-flight I/O on
    // Postgres. This is what turns a clean SIGTERM into a clean
    // exit (no half-committed transactions, no dangling server-side
    // sessions).
    state.registry.close_all().await;
    info!("bye");
    // Flush spans still queued in the batch exporter, bounded so a hung
    // collector cannot outlast the container's stop grace period (no-op
    // when export is off).
    resql::observability::shutdown(tracer_provider).await;
    Ok(())
}

/// Fleet stronghold §8.2 — pre-boot health check. Reads and parses
/// the config, runs every safety check that `serve` runs (semantic
/// validation via `Config::from_path` -> `validate()`, then
/// `validate_runtime_posture()` for fleet §3.1), and prints
/// compat-shim diagnostics in the same order `serve` would emit them.
/// Never binds a port, never opens a datasource pool — this is
/// deliberately side-effect-free so ops teams can point it at a
/// candidate config in staging without disturbing anything.
///
/// Output goes to stdout for easy piping / grep; exit code is the
/// contract with CI (see `Command::Doctor` docs for the mapping).
fn doctor(config: Option<&PathBuf>, strict: bool) -> ExitCode {
    let config_path = match resolve_config_path(config) {
        Ok(p) => p,
        Err(e) => {
            println!("ERROR: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("resql doctor — config: {}", config_path.display());
    let cfg = match Config::from_path(&config_path) {
        Ok(c) => c,
        Err(e) => {
            println!("ERROR: config parse / semantic validation failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("OK  : config parsed and semantically valid");

    // Compat-shim diagnostics — informational, don't gate the exit
    // code. Same output the operator sees at `serve` boot.
    for d in &cfg.compat_diagnostics {
        let level = match d.level {
            DiagLevel::Warn => "WARN",
            DiagLevel::Info => "INFO",
        };
        println!("{level}: [compat] {}: {}", d.source_field, d.message);
    }

    // Security-posture WARN catalogue (FN1 + fleet §8.1). One line per
    // knowingly permissive knob; same catalogue `serve` emits at boot.
    let mut warnings = 0usize;
    for w in cfg.security_warnings() {
        warnings += 1;
        println!("WARN: [security] {}: {}", w.field, w.message);
    }

    match cfg.validate_runtime_posture() {
        Ok(()) => {
            println!("OK  : runtime-posture check (fleet §3.1)");
        }
        Err(e) => {
            println!("ERROR: runtime-posture check (fleet §3.1) failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    // OTLP trace export is configured through the standard OTEL_* variables,
    // not the config file; surface it here so a deployment can be vetted
    // end to end. Nothing is contacted.
    if let Some(export) = resql::observability::export_status_from_env() {
        println!(
            "INFO: [otel] OTLP trace export enabled: endpoint={} protocol={} service.name={} export_timeout_ms={}",
            export.endpoint,
            export.transport.as_str(),
            export.service_name,
            export.timeout.as_millis()
        );
        for warning in &export.warnings {
            warnings += 1;
            println!("WARN: [otel] {warning}");
        }
    }

    println!("\ndoctor summary: {warnings} warning(s), errors: 0. strict={strict}",);
    if warnings > 0 && strict {
        // Exit 2 (distinct from "hard error" 1) so CI can distinguish
        // "config is warning-clean" from "config actually refuses to
        // boot". Only fires with --strict; otherwise warnings are
        // informational.
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

/// Install the global subscriber. Returns the OTLP tracer provider when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` enables trace export (see
/// `resql::observability`); the caller shuts it down on exit.
fn init_tracing(level: &str, format: &str) -> Option<opentelemetry_sdk::trace::TracerProvider> {
    // RESQL_LOG env overrides YAML level; RESQL_LOG_FORMAT overrides
    // YAML format so an operator can flip a running container's log
    // shape (text ↔ json) without editing the config file.
    let env_directive = std::env::var("RESQL_LOG").unwrap_or_else(|_| level.to_string());
    let filter = tracing_subscriber::EnvFilter::try_new(env_directive)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let effective_format = std::env::var("RESQL_LOG_FORMAT")
        .ok()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| format.to_ascii_lowercase());

    // `Ok(None)` when export is off; an exporter that cannot be built is
    // reported below, once the subscriber exists, and the service runs on
    // with local logging only.
    let (provider, init_error) = match resql::observability::init_provider_from_env() {
        Ok(p) => (p, None),
        Err(e) => (None, Some(e)),
    };
    // The log filter applies to the log layer only: what is logged never
    // decides what is exported (see `observability::build_dispatch`).
    let dispatch = resql::observability::build_dispatch(
        filter,
        &effective_format,
        std::io::stdout,
        provider.as_ref(),
    );
    let _ = tracing::dispatcher::set_global_default(dispatch);
    resql::observability::log_export_status(provider.is_some(), init_error.as_deref());
    provider
}

/// Container-native health check. Point `HEALTHCHECK CMD ["/app/resql",
/// "health"]` at this. Uses only `std::net::TcpStream` and a hand-rolled
/// HTTP/1.0 GET so the runtime image can be `distroless/static` — no
/// libc, no shell, no curl, no wget. Returns 0 on 2xx, 1 otherwise;
/// prints one diagnostic line on stderr so `docker inspect
/// --format '{{.State.Health}}'` shows something meaningful when a
/// probe fails.
fn health_check(url: &str, timeout_ms: u64) -> ExitCode {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => {
            eprintln!("health: only http:// URLs supported (got {url})");
            return ExitCode::FAILURE;
        }
    };
    let (authority, path) = rest
        .split_once('/')
        .map(|(a, p)| (a, format!("/{p}")))
        .unwrap_or((rest, "/".to_string()));
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p)))
        .unwrap_or((authority, 80u16));

    let timeout = Duration::from_millis(timeout_ms);
    let addr = match (host, port).to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => {
                eprintln!("health: no addresses for {host}:{port}");
                return ExitCode::FAILURE;
            }
        },
        Err(e) => {
            eprintln!("health: resolve {host}:{port} failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("health: connect {addr} failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(req.as_bytes()) {
        eprintln!("health: send failed: {e}");
        return ExitCode::FAILURE;
    }

    // Cap the read so a misbehaving server can't keep this probe alive
    // past its timeout with a slow trickle.
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 256];
    while buf.len() < 4096 {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => {
                eprintln!("health: read failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let head = std::str::from_utf8(&buf).unwrap_or("");
    let first_line = head.lines().next().unwrap_or("");
    let code: u16 = first_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    if (200..300).contains(&code) {
        ExitCode::SUCCESS
    } else {
        eprintln!("health: status {code}");
        ExitCode::FAILURE
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("register SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
