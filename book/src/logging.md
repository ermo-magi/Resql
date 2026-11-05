# Logging & observability

Resql emits structured logs on every request. Field names follow the
[OpenTelemetry HTTP semantic conventions][otel] so a JSON log store
(Loki, ELK, CloudWatch) can group and filter without custom parsing.

[otel]: https://opentelemetry.io/docs/specs/semconv/http/http-spans/

## What you get out of the box

- **One INFO line per completed request** with method, route, status,
  duration, project, trace id.
- **A W3C `traceparent`** adopted from the caller (or generated
  server-side if absent) so every log line, spanning through the
  request, carries the same 32-hex `trace_id`.
- **`X-Trace-Id` echoed on every response** so callers can correlate
  their side of the call with the server's logs.
- **Every 4xx / 5xx error** surfaces as a WARN or ERROR line naming
  the exception class (`error.kind`) alongside the HTTP status.
- **CRLF-safe log values** — attacker-controlled headers or body
  fields can't splice fake log lines.
- **JSON output** togglable per process without a config-file edit.

Health probes (`/health`, `/healthz`) are excluded from the access log
so they don't bury real traffic.

## Sample output

Text format (default) — one line per request:

```
INFO http_request{http.request.method=GET http.route=/users/hello resql.project=users trace_id=d689b658ecf3445aa9974dffa890c939}: resql::server: http request completed http.request.method=GET http.route=/users/hello http.response.status_code=200 duration_ms=0.36 resql.project=users trace_id=d689b658ecf3445aa9974dffa890c939
```

For a 400 (missing required param) you get two correlated lines with
the same `trace_id`:

```
WARN http_request{...trace_id=bf1b7244...}: resql::error: No value supplied for the SQL parameter 'name'... error.kind=InvalidDataAccessApiUsageException http.response.status_code=400
INFO http_request{...trace_id=bf1b7244...}: resql::server: http request completed ... http.response.status_code=400 duration_ms=0.19 ...
```

JSON format (`RESQL_LOG_FORMAT=json`) — one JSON object per event:

```json
{"timestamp":"2026-08-25T23:20:29.050628Z","level":"INFO","fields":{"message":"http request completed","http.request.method":"GET","http.route":"/users/hello","http.response.status_code":200,"duration_ms":1.11,"resql.project":"users","trace_id":"385b870f3a27417cbfb3dad878137014"},"target":"resql::server","span":{"http.request.method":"GET","http.route":"/users/hello","resql.project":"users","trace_id":"385b870f3a27417cbfb3dad878137014","name":"http_request"}}
```

## Field vocabulary

| Field | Semantic convention | Where |
|---|---|---|
| `http.request.method` | `http.request.method` | request span, access log, 5xx warn |
| `http.route` | `http.route` | request span, access log |
| `http.response.status_code` | `http.response.status_code` | access log, error log |
| `duration_ms` | (Resql-specific — OTel uses `duration` in ns for spans) | access log |
| `resql.project` | (Resql-specific — URL project prefix) | request span, access log |
| `trace_id` | 32-hex from W3C `traceparent` | every request-scoped line |
| `error.kind` | Java-canonical exception class name | error log lines |

The request span acts as an implicit MDC — any `tracing::info!`,
`warn!`, or `error!` fired during the request inherits these fields
in JSON output.

## Configuration

Under `logging:` in `resql.yaml`. Every field optional; sane defaults
mean you can ship without touching the block.

```yaml
logging:
  # tracing EnvFilter directive. RESQL_LOG env overrides.
  level: "info,resql=debug"

  # `text` | `json`. RESQL_LOG_FORMAT env overrides.
  format: text

  # One INFO line per completed request. On by default.
  access_log: true

  # Walk error `source()` chain (5-hop bound) into the error log
  # line. Off by default — Display is usually enough and driver
  # error chains can leak schema details.
  print_stack_trace: false

  # Cap on any body content that ends up in a log line.
  max_body_bytes: 2048

  # JSON body field names replaced with `[REDACTED]` in any logged
  # body. Case-insensitive, applied at every nesting depth.
  redact_body_fields:
    - password
    - pass
    - secret
    - token
    - access_token
    - refresh_token
    - api_key
    - authorization
```

### Env-var overrides

| Env var | Overrides | Values |
|---|---|---|
| `RESQL_LOG` | `logging.level` | any `tracing` `EnvFilter` directive |
| `RESQL_LOG_FORMAT` | `logging.format` | `text` \| `json` |

## Redaction & log-injection defence

Log fields go through two layers before hitting the log line:

1. **CRLF stripping** — every string field is sanitised with
   `sanitize_log_value`, which replaces `\r` and `\n` with a space.
   Prevents a request-header value like `foo\nWARN evil-log-line` from
   forging an extra log entry.
2. **Field-name redaction** — any JSON body that Resql chooses to log
   (currently: none by default; opt-in via a future
   `display_request_content` toggle) is walked recursively and any
   key matching `redact_body_fields` (case-insensitive) is replaced
   with `"[REDACTED]"`. Nested secrets under a redacted key are
   collapsed to the sentinel too — a redacted key means the whole
   sub-tree is gone.

Both apply regardless of `format:` — JSON output is not a bypass.

## Correlating client + server

The `X-Trace-Id` response header carries the same 32-hex trace id
that appears on every server log line for the request. Log it from
your client and grep the server logs with:

```bash
# Server side:
kubectl logs resql-xyz | grep trace_id=abc123...
# or, on JSON logs:
kubectl logs resql-xyz | jq 'select(.fields.trace_id == "abc123...")'
```

If your client already generates a W3C `traceparent` (Envoy, Istio,
otel-instrumented client SDK), Resql adopts it verbatim — no
`x-trace-id` header rewriting needed. If not, Resql generates one on
first sight and echoes it in `X-Trace-Id`; wire it into your client's
logs and end-to-end correlation just works.

## Trace export (OTLP)

Off by default. Setting `OTEL_EXPORTER_OTLP_ENDPOINT` makes Resql export
spans to any OpenTelemetry collector or OTLP-compatible backend. With the
variable unset, nothing changes: no spans are exported and log output is
identical to a build without this feature.

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Enables export. gRPC: `http://collector:4317`. HTTP: `http(s)://collector:4318` (Resql appends `/v1/traces`). |
| `OTEL_EXPORTER_OTLP_PROTOCOL` / `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | `grpc` (default) or `http/protobuf`. The per-signal variable wins; other values fall back to `grpc` with a warning. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Exporter headers, e.g. `authorization=Bearer%20<token>`. Values are never logged. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in milliseconds. |
| `OTEL_SERVICE_NAME` | `service.name` resource attribute. Default `resql`. |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, e.g. `deployment.environment=staging`. `service.name` and `service.version` from Resql win. |

TLS: use `http/protobuf` with an `https://` endpoint (rustls with the
bundled Mozilla root set; a private CA is not trusted). The gRPC exporter
is plaintext; an `https://`
endpoint with `grpc` logs a boot warning and exports will fail. For a TLS
backend over gRPC, run a collector next to Resql and let it forward.

If the collector is down, requests are unaffected; spans are dropped
from the bounded export queue.

### Spans

**Request span** (kind `SERVER`), one per request:

- name `"{METHOD} {http.route}"`, e.g. `GET /users/hello`;
- `http.route` is a template, never the raw request path: the saved-query
  path (`/<project>/<query>`, `/<project>/<query>/batch`) when the query
  exists, otherwise the matched route (`/:project/*tail`) or `unmatched`;
- `http.request.method`, `http.response.status_code`, and
  `resql.project` for a resolved saved query;
- parented on the inbound W3C `traceparent` when present, so the span
  joins the caller's trace;
- status `ERROR` and `error.type` = status code for 5xx only (4xx is a
  client error, not a server failure).

With export on, the `traceparent` returned to the caller (and seen by
anything downstream) carries Resql's own span id under the same trace
id. `X-Trace-Id` and the `trace_id` log field are unchanged; log lines
inside the request additionally carry the span's `span_id`.

`/health` and `/healthz` produce no spans: probes would dominate the
exported data.

**Query span** (kind `CLIENT`), one per saved-query execution, child of
the request span:

| Attribute | Value |
|---|---|
| name | `"{db.operation.name} {project}/{query}"`, e.g. `SELECT users/hello`; just `{project}/{query}` when the operation is not known |
| `db.system`, `db.system.name` | `postgresql`, `sqlite` or `mysql` (MariaDB reports `mysql`) |
| `db.namespace` | datasource name |
| `db.operation.name` | leading keyword (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `CALL`); omitted otherwise, e.g. for `WITH …` |
| `resql.query` | `<project>/<saved-query>` |
| `resql.rows` | rows returned (batch: total over all parameter sets) |
| `db.operation.batch.size` | number of parameter sets, batch endpoints only |
| `error.type` | the `X-Resql-Error-Code` value; span status `ERROR` |

### Never exported

- SQL text and bind parameter values.
- Request and response bodies.
- Header values (`Authorization`, cookies, tokens).
- Query strings.
- Log events. Only Resql's own spans are exported; log lines, including
  driver events that can carry SQL text, stay in the log stream and
  correlate through `trace_id`. Spans from dependencies (HTTP stack,
  exporter transport) are not exported either.

## What's not (yet) here

- **Connection-pool wait in spans.** The query span covers pool
  acquire + execution together; there is no separate pool-wait span or
  attribute yet.
- **Access-log body inclusion.** Ruuter-on-Rust has toggles for
  `display_request_content` / `display_response_content` on step
  execution; Resql's request path is simpler (no DSL / no steps) so
  the equivalent knobs aren't wired yet.
