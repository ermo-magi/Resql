# Configuration

Resql reads a single YAML file at startup. The path is set with
`--config <path>` or the `RESQL_CONFIG` env var (default `/app/resql.yaml`
inside the container).

All fields have safe defaults except `sql_dir`. Every unknown YAML field
is rejected — typos fail startup instead of silently ignoring config.

> **Test a candidate config without booting the service**:
> `resql doctor -c path/to/resql.yaml` parses, runs every safety check
> `serve` runs, and exits `0` (clean), `1` (hard error), or `2`
> (warnings only, with `--strict`). Never binds a port, never opens a
> pool. See [Fleet §8.2 doctor CLI](#doctor-cli).

## Top-level fields

| Field | Type | Default | Purpose |
|---|---|---|---|
| `sql_dir` | path | `./templates/` (Java-compat) | Directory scanned for `.sql` files at startup. |
| `server` | object | see below | HTTP listener settings. |
| `datasources` | list | `[]` | Configured database connections. |
| `project_datasource_map` | map | `{}` | Override which datasource a URL project routes to. |
| `allow_datasource_header` | bool | `false` | Honour `X-Datasource: <name>` on requests (R1). |
| `datasource_header_allowlist` | map | `{}` | Per-project allow-list for `X-Datasource` overrides. |
| `default_datasource` | string | *(unset)* | Fallback for the Java-legacy `POST /:name/batch` URL shape. |
| `cors` | object | see below | CORS layer settings. |
| `admin` | object | see below | Admin-endpoint gates (`/datasources`, `/openapi.json`). |
| `security` | object | see below | Inter-service bearer + non-loopback boot posture. |
| `rate_limit` | object | see below | Optional global rate limiter (R8). |
| `logging` | object | see below | Log level + format + redaction. |
| `openapi` | object | see below | Cosmetic tuning for the generated OpenAPI spec. |

## `server`

| Field | Type | Default | Purpose |
|---|---|---|---|
| `bind` | string | `0.0.0.0:8080` | Listener address. |
| `max_body_bytes` | integer | `1048576` (1 MiB) | Inbound JSON body cap. Overflow → HTTP 413 in the header-envelope shape. |
| `request_timeout_seconds` | integer | `30` | Per-request wall-clock cap. 0 refused at boot; overflow → HTTP 504. Postgres pools also `SET statement_timeout` to this value; MySQL pools `SET SESSION MAX_EXECUTION_TIME` (SELECT-only, hint-shaped — middleware is still the authoritative cap). |

## `datasources` (list of)

| Field | Type | Default | Purpose |
|---|---|---|---|
| `name` | string | *(required)* | Unique label. Referenced from URL project or `X-Datasource` header. |
| `url` | string | *(required)* | Connection URL. Supported schemes: `postgres://`, `postgresql://`, `mysql://`, `mariadb://`, `sqlite:`. |
| `username` | string | `""` | Username to inject in the URL userinfo. Overrides any `user:` already in the URL — the config-supplied value wins (see [issue #27](https://github.com/turnerrainer/Resql/issues/27)). |
| `password_env` | string | `""` | Env-var **name** holding the password. **Never store passwords in this file.** |
| `password` | string | `null` | Java-compat plaintext form. If set, compat-shim emits WARN every time. Rejected if `password_env` is also set. |
| `max_connections` | integer | `10` | Pool size ceiling. |
| `acquire_timeout_seconds` | integer | `5` | Per-acquire wait. |

Startup refuses in any of these cases:

- Two datasources share a `name`.
- A `username` is set but both `password` and `password_env` are empty.
- Both `password` and `password_env` are set (unambiguous choice required).
- `password_env` names a variable that is not set in the environment.
- `project_datasource_map` refers to a name absent from `datasources`.
- `datasource_header_allowlist` names a datasource absent from `datasources`.
- `server.request_timeout_seconds` is `0` (would allow slow-query pool exhaustion).

## `project_datasource_map`

Maps URL project segment → datasource name. When a request arrives at
`POST /crm/find-user`, the default behaviour is to look up a datasource
named `crm`. Add an entry `crm: primary-db` to route it elsewhere.

Falls back to the project name if the map has no entry — a bare
`sql_dir` layout of `sql/foo/…` will look up datasource `foo` with no
config.

## `allow_datasource_header` + `datasource_header_allowlist` (R1)

By default (`allow_datasource_header: false`), the `X-Datasource: <name>`
request header is ignored. This is the safe posture — allowing header
routing is a lateral-move lane inside the trust boundary.

To enable header routing, set both fields:

```yaml
allow_datasource_header: true
datasource_header_allowlist:
  crm:   [crm, crm_replica]     # project `crm` may override to these two
  audit: [audit]                # project `audit` may override only to itself
```

Any (project, requested-datasource) pair not in the allow-list returns
`403 ForbiddenDatasourceOverrideException`. The rejection message names
only the offender — attackers cannot enumerate the registry by probing.

## `cors`

| Field | Type | Default | Purpose |
|---|---|---|---|
| `allowed_origins` | string | `""` (empty) | Empty → no CORS layer at all (browsers block cross-origin). `*` → wildcard (WARN at boot). Comma-separated list of exact origins otherwise. |

When configured, the layer advertises only `GET, POST` methods and only
the request headers Resql actually reads
(`content-type, authorization, x-datasource, traceparent`).

## `admin` — endpoint gates

| Field | Type | Default | Purpose |
|---|---|---|---|
| `datasources_public` | bool | `false` | When `true`, `GET /datasources` returns the (redacted) list of registered pools. `false` → 404 (indistinguishable from a non-mounted route). |
| `openapi_public` | bool | `false` | When `true`, `GET /openapi.json` returns the generated OpenAPI spec. `false` → 404 (FN3). |

Both endpoints leak recon surface to unauth callers — leave both
`false` in production unless a proxy authenticates upstream.

## `security` — inter-service bearer + boot posture

| Field | Type | Default | Purpose |
|---|---|---|---|
| `inter_service_token_env` | string | `""` | Env-var **name** whose value is the shared secret. When set, every request except `/health` / `/healthz` MUST carry `Authorization: Bearer <value>` or receive 401. Boot refuses if the env var is unset or empty. |
| `trust_network` | bool | `false` | Opt-out for the boot-time refuse-on-non-loopback check. Certifies that a reverse proxy authenticates every request before it reaches Resql. |

Boot refuses to start if `server.bind` is non-loopback AND
`inter_service_token_env` is empty AND `trust_network` is false. The
three ways to satisfy the check:

1. Bind to loopback (`127.0.0.1`, `[::1]`, `localhost`) and let a
   reverse proxy forward inbound traffic.
2. Set `inter_service_token_env` — enables the built-in bearer gate.
3. Set `trust_network: true` — certifies proxy-upstream auth.

Token comparison is constant-time on equal-length inputs. Rejection
logs never echo the attempted token content.

## `rate_limit` — global token bucket

| Field | Type | Default | Purpose |
|---|---|---|---|
| `requests_per_second` | integer | `0` | Sustained rate cap. `0` disables the middleware entirely (zero cost). |
| `burst` | integer | `0` | Bucket size. `0` auto-sizes to `2 * requests_per_second`. Must be `>=` rps when both non-zero. |

Off by default. Meant for deployments that face callers directly; if
Resql sits behind a proxy that already rate-limits per real caller,
leave this off. When enabled, exhaustion returns
`429 Too Many Requests` in the header envelope (`X-Resql-Error-Code:
TooManyRequestsException`) with an RFC 6585 `Retry-After` header.
Health probes bypass.

**Scope is global (per-process), not per-IP** — Resql almost always
sees a reverse proxy as the caller, so per-IP buckets would collapse
to a single-key cache.

## `logging`

| Field | Type | Default | Purpose |
|---|---|---|---|
| `level` | string | `info,resql=debug` | `tracing_subscriber` EnvFilter directive. |
| `format` | string | `text` | `text` or `json`. |
| `access_log` | bool | `true` | Emit one INFO line per completed request. |
| `print_stack_trace` | bool | `false` | Include error `source()` chain on WARN. **Off in production** — chains can leak schema names. |
| `max_body_bytes` | integer | `2048` | Cap on body-content included in a log line. |
| `redact_body_fields` | list | `[password, pass, secret, token, access_token, refresh_token, api_key, authorization]` | Field names (case-insensitive) redacted at every nesting depth. |

The `RESQL_LOG` env var overrides `level` at runtime; `RESQL_LOG_FORMAT`
overrides `format`. Neither touches the config file.

OTLP trace export is configured with the standard `OTEL_*` environment
variables, not this file; see [Trace export (OTLP)](./logging.md#trace-export-otlp).
It is independent of `level`.

## `openapi`

| Field | Type | Default | Purpose |
|---|---|---|---|
| `title` | string | `Resql` | Rendered as `info.title` in the spec. |
| `description` | string | *(generic)* | Rendered as `info.description`. |
| `server_url` | string | `/` | Rendered as the single `servers[0].url`. |

Only affects the spec `/openapi.json` returns (when the gate is open).
Every field is optional.

## Boot-time WARN catalogue

Every knowingly permissive knob emits a WARN at boot (fleet §8.1). The
current list:

- `cors.allowed_origins == "*"` — every origin can read every response
  cross-origin.
- `server.bind` non-loopback — see the boot-refuse discussion above.
- `admin.datasources_public: true` — leaks backend topology.
- `logging.print_stack_trace: true` — schema-name leak in error chains.
- Any datasource carrying a plaintext `password:` (steer to
  `password_env`).

Nothing here changes behaviour; ops teams see the same audit line
they'd see in a review. Also visible via `resql doctor`.

## `doctor` CLI

Pre-boot health check (fleet §8.2). Parses the config, runs every
safety check `serve` runs, prints WARN entries + compat-shim
diagnostics, and exits with a CI-friendly code:

```
$ resql doctor -c /path/to/resql.yaml
resql doctor — config: /path/to/resql.yaml
OK  : config parsed and semantically valid
WARN: [security] cors.allowed_origins: cors.allowed_origins is "*" — every origin ...
OK  : runtime-posture check (fleet §3.1)

doctor summary: 1 warning(s), errors: 0. strict=false
$ echo $?
0

$ resql doctor --strict -c /path/to/resql.yaml
...
$ echo $?
2
```

- Exit `0` — every check passed.
- Exit `1` — hard error (parse failure, semantic validation, or
  runtime-posture refuse).
- Exit `2` — warnings only, with `--strict`.

When `OTEL_EXPORTER_OTLP_ENDPOINT` is set in the environment, doctor also
prints an `INFO: [otel]` line describing the export and one `WARN: [otel]`
line per problem (see [Trace export](./logging.md#trace-export-otlp)); those
count towards `--strict`.

Never binds a port, never opens a datasource pool, never contacts a
collector. Safe to run against a production config file.

## Env-var overrides

Any string in `password_env` and `security.inter_service_token_env` is
looked up in the process environment. Nothing else is env-driven — the
config file is authoritative. If you need per-environment overrides,
use one config file per environment or mount a config file from a
secret at runtime.

## Complete example — proxy-fronted internal service

```yaml
server:
  bind: "127.0.0.1:8080"     # loopback → satisfies §3.1 without extra config
  max_body_bytes: 2097152    # 2 MiB
  request_timeout_seconds: 30

sql_dir: "/app/sql"

project_datasource_map:
  legacyapp: primary
  reports:   analytics

datasources:
  - name: primary
    url:          "postgres://pg-primary.internal:5432/appdb"
    username:     "resql"
    password_env: "RESQL_PRIMARY_PASSWORD"
    max_connections: 20
  - name: analytics
    url:          "postgres://pg-analytics.internal:5432/warehouse"
    username:     "resql_ro"
    password_env: "RESQL_ANALYTICS_PASSWORD"
    max_connections: 5

# CORS closed by default; no browser talks to Resql directly.
cors:
  allowed_origins: ""

admin:
  datasources_public: false    # 404 for unauth callers
  openapi_public:     false    # 404 for unauth callers

logging:
  level:  "info,resql=debug,sqlx=warn"
  format: "json"
```

## Complete example — direct-exposure deployment

Same shape as above, but Resql binds a public interface. The bearer
gate is enabled; boot refuses to start until `RESQL_INTER_SERVICE_TOKEN`
is set.

```yaml
server:
  bind: "0.0.0.0:8080"

sql_dir: "/app/sql"

datasources:
  - name: primary
    url:          "postgres://pg-primary.internal:5432/appdb"
    username:     "resql"
    password_env: "RESQL_PRIMARY_PASSWORD"

# Non-loopback bind → §3.1 boot-refuse triggers unless
# security.inter_service_token_env is set OR security.trust_network is true.
security:
  inter_service_token_env: "RESQL_INTER_SERVICE_TOKEN"

# R8: sustain 100 rps, absorb bursts up to 200. Off by default; turn on
# when Resql is directly exposed.
rate_limit:
  requests_per_second: 100
  burst: 200

logging:
  level:  "info,resql=debug"
  format: "json"
```

Set the env var before boot:

```bash
export RESQL_INTER_SERVICE_TOKEN="$(openssl rand -base64 32)"
export RESQL_PRIMARY_PASSWORD="…"
resql -c /app/resql.yaml
```

Every request except `/health` / `/healthz` must now carry
`Authorization: Bearer $RESQL_INTER_SERVICE_TOKEN`.

## MySQL / MariaDB backend

MySQL and MariaDB are first-class alongside Postgres. Use `mysql://`
or `mariadb://` as the datasource URL scheme.

```yaml
datasources:
  - name: analytics
    url:          "mysql://analytics-db.internal:3306/analytics"
    username:     "resql_analytics"
    password_env: "RESQL_ANALYTICS_PASSWORD"
```

**The authoritative reference lives on its own page**:
[MySQL / MariaDB backend](./mysql-setup.md) — includes the Postgres →
MySQL dialect cheat sheet, the array-as-JSON contract, the full row →
JSON type map, the `MAX_EXECUTION_TIME` timeout caveat, local
development recipe, and an LLM authoring checklist. Read that page
before writing any `.sql` file for a MySQL datasource.
