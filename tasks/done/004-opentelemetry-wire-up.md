# 004 — OpenTelemetry wire-up

## Filed
2026-07-29 — JVM Resql had OTel deps but nothing was actually
instrumented. Rust rewrite dropped the deps entirely to avoid the
same fig-leaf state. This task adds them back and instruments the
things that matter.

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
