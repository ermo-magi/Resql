# 004 — OpenTelemetry wire-up

## Filed
2026-07-29 — JVM Resql had OTel deps but nothing was actually
instrumented. Rust rewrite dropped the deps entirely to avoid the
same fig-leaf state. This task adds them back and instruments the
things that matter.

## Landed
2026-11-12 — PR #72. Feature commits by Ermo Mägi (`e75f49d`, `b3e0df0`,
`dac02a7`); review follow-up commits on the same PR fix two defects found
in review and close the gaps below.

Shipped: request (SERVER) and saved-query (CLIENT) spans exported over
OTLP/gRPC or OTLP/HTTP, parented on the inbound W3C `traceparent`; no SQL
text, bind values, bodies, headers or query strings in spans
(`book/src/logging.md`, "Trace export (OTLP)"; `DIVERGENCES.md` DIV-024).

Review follow-ups:

- Export no longer depends on the log level. The log filter sat on the
  registry and also gated the OTel layer, so `RESQL_LOG=warn` silently
  exported nothing. It is now a per-layer filter on the log layer only
  (`observability::build_dispatch`).
- Exports and shutdown are bounded. `opentelemetry-otlp` 0.27 never
  applied its HTTP timeout, so a hung collector held each export for the
  batch processor's 30 s cap and SIGTERM-to-exit took ~30 s (SIGKILL under
  `docker stop`). The reqwest client now carries the timeout
  (`OTEL_EXPORTER_OTLP[_TRACES]_TIMEOUT`, default 10 s) and the final flush
  is capped at 5 s (`observability::shutdown`).
- Boot `INFO` line when export is on; `WARN` for https + gRPC, plaintext
  exporter headers to a non-loopback collector, bad protocol / timeout
  values; the same in `resql doctor` (`--strict` counts them). Endpoints
  carrying credentials are refused, because exporter errors echo the URL.
  Export failures are logged (OTel `internal-logs` feature) without header
  values.
- Test fixes: the env-mutating test moved to its own binary (it set
  `OTEL_TRACES_SAMPLER`, which every `TracerProvider` built in parallel
  reads), and span-count assertions wait boundedly for the request span,
  which sqlx-sqlite's worker thread can close just after the response (the
  original `request_span_route_is_a_template_never_the_raw_path` failed
  4/60 runs on the PR head; 0/100 after).

Tests: `integration_otel_export.rs` (in-process spans),
`integration_otel_env.rs` (env contract incl. sampler, timeout,
credentials), `integration_otel_log_filter.rs` (export at every log
level), `integration_otel_exporter.rs` (timeout + shutdown deadline
against hung / healthy collectors), `integration_otel_process.rs` (the
real binary against real TCP collectors: levels, boot lines, leaks,
SIGTERM timing), `integration_log_output.rs` (export-off log output
identical to the previous wiring, every level, both formats),
`integration_doctor_cli.rs` (`[otel]` lines + `--strict`). Each fix was
mutation-checked: reverting it makes its guard test fail.

### Deviations from the design below

- **No `otel` cargo feature; the OTel stack always compiles in.** Export
  is a runtime switch (`OTEL_EXPORTER_OTLP_ENDPOINT`), like Ruuter-on-Rust,
  so one published image serves both postures and the fleet shares one
  contract. Cost: ~25 extra crates (tonic, prost, opentelemetry*), all
  inside the `deny.toml` licence allow-list, no OpenSSL. Revisit if image
  size or the advisory surface becomes a problem.
- **No `telemetry:` block in `resql.yaml`; standard `OTEL_*` variables
  instead.** They are what collectors, Helm charts and the OTel docs
  expect, and what Ruuter-on-Rust reads. `sample_ratio` is
  `OTEL_TRACES_SAMPLER=parentbased_traceidratio` + `OTEL_TRACES_SAMPLER_ARG`.
- **No `tower_http::trace::TraceLayer`.** The existing
  `request_observability` middleware owns the request span already; a
  second layer would duplicate it.
- **Docs extend `book/src/logging.md` instead of a new
  `observability.md`.** DEV-REQUIREMENTS §4.1 caps feature chapters, and
  the logging chapter already documented `traceparent` and `trace_id`.
- **No with/without-feature compile test**, because there is no feature.
  The export-off path is covered behaviourally instead
  (`integration_log_output.rs`, `export_off_logs_no_otel_line_and_contacts_nobody`).

## Severity
Medium. Consumers who deploy behind an OTLP collector currently see
no request-level traces from Resql.

## Motivation
Distributed tracing across a call graph (`upstream → resql → postgres`)
is the difference between "we know the request failed" and "we know
which pool exhaustion event caused it." Ship it as first-class.

## Fix / Design
Add `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, and
`tracing-opentelemetry` deps behind an `otel` feature flag. Default
off — no dep bloat unless enabled.

Config:

```yaml
telemetry:
  otlp_endpoint: "http://otel-collector.internal:4317"
  service_name: "resql"
  sample_ratio: 0.1
```

Instrumentation:

- One span per HTTP request (auto via `tower_http::trace::TraceLayer`).
- Child span per SQL execution: attributes `project`, `endpoint`,
  `datasource`, `rows_returned`, `error.kind` on failure.
- No bound parameter values in span attributes — PII risk.

## Acceptance
- [ ] `otel` feature flag defined; without it, no OTel deps compile in.
- [ ] Enabling emits spans to configured OTLP endpoint.
- [ ] SQL span excludes bound parameter values.
- [ ] Fresh docs chapter `book/src/observability.md` explaining setup.
- [ ] Regression test verifying compile succeeds with and without the feature.

## Estimated effort
1.5 days.

## Dependencies
Task 002.

## Non-scope
- Metrics (separate task if requested).
- Log correlation IDs beyond what `tracing` already provides.

## Risks
- OTel Rust SDK API instability. Mitigation: pin to a specific SDK
  minor version; regenerate lockfile only on explicit upgrade tasks.
