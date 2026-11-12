//! Fleet stronghold §8.2: `resql doctor` — pre-boot health check.
//!
//! Spawn the compiled binary (via `env!("CARGO_BIN_EXE_resql")`) and
//! verify the exit-code contract:
//!   0 — every check passed
//!   1 — hard error (parse failure or runtime-posture refuse)
//!   2 — warnings only, `--strict` flag set
//!
//! No listener is opened. No datasource pool is created. Ops teams
//! wire this into CI as a non-blocking check (drop `--strict`) or as
//! a blocking gate (add `--strict`).

use std::io::Write;
use std::path::Path;
use std::process::Command;

fn resql_bin() -> &'static str {
    env!("CARGO_BIN_EXE_resql")
}

fn write_config(contents: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .prefix("resql-doctor-")
        .suffix(".yaml")
        .tempfile()
        .unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn doctor_exits_zero_on_loopback_bind() {
    let cfg = write_config("sql_dir: ./sql\nserver:\n  bind: \"127.0.0.1:8080\"\n");
    let out = Command::new(resql_bin())
        .args(["doctor", "-c"])
        .arg(cfg.path())
        .output()
        .expect("spawn doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "expected exit 0, got {}: stdout={stdout}",
        out.status
    );
    assert!(stdout.contains("OK  : config parsed"));
    assert!(stdout.contains("OK  : runtime-posture check"));
}

#[test]
fn doctor_exits_one_on_non_loopback_bind_without_auth() {
    // Fleet §3.1: this is the F-RES-3 shape. Doctor must fail 1.
    let cfg = write_config("sql_dir: ./sql\nserver:\n  bind: \"0.0.0.0:8080\"\n");
    let out = Command::new(resql_bin())
        .args(["doctor", "-c"])
        .arg(cfg.path())
        .output()
        .expect("spawn doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1, got {}: stdout={stdout}",
        out.status
    );
    assert!(stdout.contains("ERROR: runtime-posture"));
    assert!(stdout.contains("non-loopback bind"));
}

#[test]
fn doctor_exits_one_on_config_parse_failure() {
    let cfg = write_config("not: valid: yaml: at: all: [\n");
    let out = Command::new(resql_bin())
        .args(["doctor", "-c"])
        .arg(cfg.path())
        .output()
        .expect("spawn doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1, got {}: stdout={stdout}",
        out.status
    );
    assert!(stdout.contains("ERROR: config parse"));
}

/// `doctor` with a clean slate of OTel/Resql log variables plus `env`.
fn doctor_with_env(cfg: &Path, strict: bool, env: &[(&str, &str)]) -> (Option<i32>, String) {
    let mut cmd = Command::new(resql_bin());
    cmd.arg("doctor");
    if strict {
        cmd.arg("--strict");
    }
    cmd.arg("-c").arg(cfg);
    for (key, _) in std::env::vars() {
        if key.starts_with("OTEL_") {
            cmd.env_remove(key);
        }
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn doctor");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

const LOOPBACK_CFG: &str = "sql_dir: ./sql\nserver:\n  bind: \"127.0.0.1:8080\"\n";

#[test]
fn doctor_says_nothing_about_otel_when_export_is_off() {
    let cfg = write_config(LOOPBACK_CFG);
    let (code, stdout) = doctor_with_env(cfg.path(), true, &[]);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(!stdout.to_ascii_lowercase().contains("otel"), "{stdout}");
}

#[test]
fn doctor_reports_a_clean_otel_export_without_warnings() {
    let cfg = write_config(LOOPBACK_CFG);
    let (code, stdout) = doctor_with_env(
        cfg.path(),
        true,
        &[
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://collector.internal:4317",
            ),
            ("OTEL_SERVICE_NAME", "orders-resql"),
        ],
    );
    assert_eq!(code, Some(0), "clean export must pass --strict: {stdout}");
    assert!(
        stdout.contains("INFO: [otel] OTLP trace export enabled: endpoint=http://collector.internal:4317 protocol=grpc service.name=orders-resql export_timeout_ms=10000"),
        "{stdout}"
    );
    assert!(stdout.contains("0 warning(s)"), "{stdout}");
}

#[test]
fn doctor_warns_about_risky_otel_settings_and_strict_fails_on_them() {
    let cfg = write_config(LOOPBACK_CFG);
    let risky: [(&[(&str, &str)], &str); 4] = [
        (
            &[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://192.0.2.1:4318"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
                (
                    "OTEL_EXPORTER_OTLP_HEADERS",
                    "authorization=Bearer%20SECRETVALUE",
                ),
            ],
            "plaintext",
        ),
        (
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector:4317")],
            "no TLS",
        ),
        (
            &[(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://user:HUNTER2@collector:4318",
            )],
            "must not carry credentials",
        ),
        (
            &[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
                ("OTEL_EXPORTER_OTLP_TIMEOUT", "ten"),
            ],
            "positive number of milliseconds",
        ),
    ];
    for (env, needle) in risky {
        let (code, stdout) = doctor_with_env(cfg.path(), true, env);
        assert_eq!(code, Some(2), "{needle}: --strict must exit 2: {stdout}");
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with("WARN: [otel]") && l.contains(needle)),
            "{needle}: {stdout}"
        );
        for secret in ["SECRETVALUE", "HUNTER2"] {
            assert!(!stdout.contains(secret), "{secret} leaked: {stdout}");
        }
        // Without --strict the same configuration is informational only.
        let (code, _) = doctor_with_env(cfg.path(), false, env);
        assert_eq!(code, Some(0), "{needle}");
    }
}

#[test]
fn doctor_help_lists_subcommand() {
    // Regression pin: fleet §8.2 requires the subcommand to be
    // discoverable via `--help` so operators can grep for it.
    let out = Command::new(resql_bin())
        .arg("--help")
        .output()
        .expect("spawn help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("doctor"), "help missing 'doctor': {stdout}");
}
