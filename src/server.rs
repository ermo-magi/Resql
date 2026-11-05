use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::Instrument;

use crate::config::{Config, LoggingConfig};
use crate::db::{DatasourceRegistry, SharedRegistry};
use crate::error::{ResqlError, ERROR_CODE_HEADER, ERROR_MESSAGE_HEADER};
use crate::health::{self, StartTime};
use crate::loader::{HttpMethod, QueryIndex};
use crate::logging::{
    generate_traceparent, sanitize_log_value, trace_id_from_traceparent, truncate_for_log,
    truncate_user_input,
};
use crate::openapi;
use crate::query;

const DATASOURCE_HEADER: &str = "x-datasource";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub index: Arc<QueryIndex>,
    pub registry: SharedRegistry,
    pub start: StartTime,
    /// OpenAPI 3.1 spec derived from the loaded QueryIndex at boot.
    /// Cached because the declaration set is fixed for the process
    /// lifetime and building it per request would be wasteful.
    pub openapi: Arc<Value>,
    /// The resolved inter-service bearer token (F-RES-3), cached at
    /// boot from `security.inter_service_token_env` so the auth
    /// middleware doesn't `getenv` per request. `None` = gate disabled.
    pub inter_service_token: Arc<Option<String>>,
    /// Optional global rate limiter (R8). `None` = disabled (default).
    /// Shared across every request via `Arc` and internally guarded by
    /// a `Mutex` — token acquisition is O(1) so contention is
    /// negligible for realistic RPS.
    pub rate_limiter: Arc<Option<RateLimiter>>,
}

/// Global token-bucket rate limiter (R8). One bucket per process,
/// scaled by 1000 internally so sub-token refill deltas don't get lost
/// to integer truncation.
pub struct RateLimiter {
    inner: std::sync::Mutex<RateLimiterState>,
    /// Refill rate scaled by 1000 (tokens * 1000 per millisecond).
    /// Precomputed at construction so acquire path just multiplies.
    refill_scaled_per_ms: i64,
    /// Effective bucket size in scaled tokens.
    burst_scaled: i64,
}

struct RateLimiterState {
    /// Current tokens available, scaled by 1000.
    tokens_scaled: i64,
    /// Last refill timestamp — milliseconds since UNIX epoch. Using
    /// wall-clock rather than a monotonic Instant so multi-thread
    /// callers see a consistent view without extra synchronisation
    /// with a reference epoch.
    last_refill_ms: i64,
}

impl RateLimiter {
    pub fn new(requests_per_second: u32, burst: u32) -> Self {
        // Callers are expected to have run `Config::effective_rate_limit_burst`
        // which auto-sizes burst=0 to 2*rps; if a caller passes bare
        // ints (tests) `burst.max(1)` guarantees at least one token
        // ever fits in the bucket. `.max()` on `burst` alone (NOT vs
        // rps) — a legitimate config might want burst < rps for
        // aggressive smoothing.
        let rps = requests_per_second.max(1) as i64;
        let burst_scaled = (burst.max(1) as i64).saturating_mul(1000);
        let refill_scaled_per_ms = rps; // rps tokens/sec = rps*1000 tokens*1000/1000ms = rps scaled/ms
        Self {
            inner: std::sync::Mutex::new(RateLimiterState {
                tokens_scaled: burst_scaled,
                last_refill_ms: now_ms(),
            }),
            refill_scaled_per_ms,
            burst_scaled,
        }
    }

    /// Try to consume one token. On success returns `Ok(())`; on
    /// exhaustion returns `Err(retry_after_secs)` — always at least 1
    /// so a caller's `Retry-After` header is a valid HTTP-delta.
    pub fn try_acquire(&self) -> Result<(), u64> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        let elapsed = (now - state.last_refill_ms).max(0);
        state.tokens_scaled = (state.tokens_scaled
            + elapsed.saturating_mul(self.refill_scaled_per_ms))
        .min(self.burst_scaled);
        state.last_refill_ms = now;
        if state.tokens_scaled >= 1000 {
            state.tokens_scaled -= 1000;
            Ok(())
        } else {
            // Deficit in scaled tokens → convert back to millis needed
            // for one full token to refill. `refill_scaled_per_ms` >= 1
            // by construction (rps >= 1 when enabled).
            let deficit_scaled = 1000 - state.tokens_scaled;
            // Inline ceil-div: `int::div_ceil` is unstable on rustc
            // 1.88 (the pinned rustfmt / clippy version), so open-code
            // it to keep the toolchain requirement stable.
            let refill = self.refill_scaled_per_ms.max(1);
            let millis_needed = (deficit_scaled + refill - 1) / refill;
            let secs = (millis_needed / 1000).max(1) as u64;
            Err(secs)
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn router(state: AppState) -> Router {
    let cors = build_cors(&state.config.cors.allowed_origins);
    let body_limit = state.config.server.max_body_bytes;
    let timeout = Duration::from_secs(state.config.server.request_timeout_seconds);
    let logging_for_layer = state.config.logging.clone();
    let bearer_for_layer = state.inter_service_token.clone();
    let rate_limiter_for_layer = state.rate_limiter.clone();

    let mut router = Router::new()
        .route("/health", get(health_handler))
        .route("/healthz", get(health_handler))
        .route("/datasources", get(list_datasources))
        .route("/openapi.json", get(openapi_handler))
        .route("/:project/*tail", get(query_get).post(query_post));

    // Only attach a CORS layer when explicitly configured. Absent
    // configuration means no `Access-Control-Allow-Origin` header on
    // responses, so browsers block cross-origin reads (R2).
    if let Some(layer) = cors {
        router = router.layer(layer);
    }

    router
        // Cap wall-clock time per request. The layer aborts the inner
        // future and returns 504 Gateway Timeout when the deadline
        // elapses — pool connections get dropped back to the pool on
        // cancellation, so a single slow query can no longer pin a
        // slot indefinitely. Postgres's own `statement_timeout` (set
        // in the connection hook — see `db::connect`) is the
        // belt-and-braces backstop that kills the server-side query
        // when the Rust future has already gone away.
        //
        // 504 (not 408) because "the upstream — the database — didn't
        // respond in time" is the accurate semantic; 408 would imply
        // the client itself was slow.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            timeout,
        ))
        .layer(RequestBodyLimitLayer::new(body_limit))
        // FN5: RequestBodyLimit + TimeoutLayer emit their responses
        // outside the router — so bare-text "length limit exceeded"
        // 413s and empty-body 504s bypass the ResqlError envelope. This
        // middleware sits OUTSIDE those two layers (so it sees their
        // responses) and inside the observability layer (so the access
        // log still fires exactly once); it rewrites any non-2xx that
        // arrives without the two error headers into the canonical
        // header-envelope shape: body becomes `[]`, X-Resql-Error-Code
        // and X-Resql-Error-Message carry the exception name + message.
        // Existing ResqlError responses already carry the headers, so
        // this middleware leaves them untouched (idempotent).
        .layer(middleware::from_fn(envelope_error_reshape))
        // F-RES-3: optional shared-secret bearer gate. When enabled it
        // sits INSIDE the observability middleware (so the access log
        // still fires for 401s) but OUTSIDE the timeout / body-limit
        // layers (so an unauth request never occupies a pool slot or
        // pays wall-clock time for a big body). Health probes bypass.
        // The bearer's own 401 already carries the envelope shape via
        // ResqlError::Unauthorized, so envelope_error_reshape below is
        // idempotent on it.
        .layer(middleware::from_fn(move |req, next| {
            let token = bearer_for_layer.clone();
            require_inter_service_bearer(token, req, next)
        }))
        // R8: optional global rate limiter. Sits OUTSIDE the bearer
        // gate + body-limit + timeout so a rate-limited request never
        // pays for any of them. `/health` and `/healthz` bypass so LB
        // liveness isn't affected. No-op when the config disables it.
        .layer(middleware::from_fn(move |req, next| {
            let limiter = rate_limiter_for_layer.clone();
            rate_limit_middleware(limiter, req, next)
        }))
        // Fleet stronghold §5.1 — set the five browser-side defence
        // headers on every response. Resql is a JSON API that sits
        // behind Ruuter, so a browser should never render its output,
        // but a reverse-proxy misconfiguration (dev, staging, or a
        // rule that accidentally serves the JSON as text/html) could
        // let a payload get evaluated. Cheap defence-in-depth.
        .layer(middleware::from_fn(security_headers_middleware))
        .layer(middleware::from_fn(move |req, next| {
            let cfg = logging_for_layer.clone();
            request_observability(cfg, req, next)
        }))
        .with_state(state)
}

/// F-RES-3 middleware. When a bearer token is configured, every
/// request except `/health` and `/healthz` MUST carry a matching
/// `Authorization: Bearer <token>` header — constant-time
/// comparison, no substring / prefix matches, no length side-channel
/// beyond the token length itself (public — the header name reveals
/// nothing). When the token is not configured, this middleware is a
/// pass-through so the default posture is unchanged (internal-only
/// service behind Ruuter).
async fn require_inter_service_bearer(
    expected: Arc<Option<String>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = expected.as_ref() else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if path == HEALTH_PATH || path == HEALTHZ_PATH {
        return next.run(req).await;
    }
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        // FLEET-STRONGHOLDS §1.4: never echo the attempted token
        // content — the WARN is structured with only the failure
        // kind. Log once per rejection so operators still see the
        // audit trail; downstream envelope-reshape middleware will
        // convert the 401 into the header envelope shape.
        tracing::warn!(
            error.kind = "UnauthorizedException",
            "inter-service bearer check failed"
        );
        return ResqlError::Unauthorized.into_response();
    }
    next.run(req).await
}

/// R8 middleware. When a global rate limiter is configured, try to
/// take a token before letting the request through. On exhaustion,
/// return `429 Too Many Requests` + a `Retry-After` header. Health
/// probes bypass the limiter so a saturated bucket doesn't flap the
/// load balancer.
async fn rate_limit_middleware(
    limiter: Arc<Option<RateLimiter>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(limiter) = limiter.as_ref() else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if path == HEALTH_PATH || path == HEALTHZ_PATH {
        return next.run(req).await;
    }
    match limiter.try_acquire() {
        Ok(()) => next.run(req).await,
        Err(retry_after_secs) => {
            tracing::warn!(
                error.kind = "TooManyRequestsException",
                retry_after_secs,
                "rate limit exceeded"
            );
            let mut resp = ResqlError::TooManyRequests { retry_after_secs }.into_response();
            if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, v);
            }
            resp
        }
    }
}

/// Constant-time byte-slice equality on the shorter of the two
/// lengths. Length mismatch rejects up front — token length is not a
/// secret; the content is. Returns `false` immediately for
/// unequal-length inputs so a caller can't feed a very long "guess"
/// and observe a length-proportional runtime hint.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Rewrite bare non-2xx responses from lower layers into the
/// header-envelope shape ResqlError uses. Any 4xx/5xx that arrives
/// without `X-Resql-Error-Code` gets:
///
/// - body swapped to the empty JSON array `[]` (so a naive
///   `body.length > 0` in a downstream DSL sees 0 rows on any error
///   and fails closed, matching issue #25),
/// - `X-Resql-Error-Code` set to the canonical exception name for the
///   status (see `envelope_kind_for_status`),
/// - `X-Resql-Error-Message` set to a printable-ASCII summary derived
///   from the original body if it looks like text, otherwise the
///   status's canonical reason phrase.
///
/// Idempotent — ResqlError responses already carry both headers, so
/// this middleware skips them. This fixes the FN5 finding: previously
/// oversize bodies returned `413 length limit exceeded` as
/// `text/plain`, while every other error path already used the
/// envelope; downstream callers had to special-case one class of
/// error.
async fn envelope_error_reshape(req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    let status = response.status();
    if status.is_success() || status.is_redirection() || status.is_informational() {
        return response;
    }
    if response.headers().contains_key(ERROR_CODE_HEADER) {
        // Already envelope-shaped (came from ResqlError::into_response).
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .unwrap_or_default();
    let kind = envelope_kind_for_status(parts.status);
    let message = derive_envelope_message(&parts, &bytes);
    // Rebuild the response with the envelope shape. Preserve original
    // status; drop the incoming body and any inherited headers that
    // would conflict with the JSON body shape.
    let mut resp = (parts.status, Json(Value::Array(Vec::new()))).into_response();
    let headers = resp.headers_mut();
    // Copy through any downstream-set headers we care about (e.g. the
    // trace id added by the observability middleware after this layer
    // — it lands on the outbound response, not this one, so we don't
    // need to copy it here; but do preserve `Retry-After` etc. that a
    // 429 might carry).
    for (name, value) in parts.headers.iter() {
        // Skip content-* because Json::into_response set its own.
        let n = name.as_str();
        if n.eq_ignore_ascii_case("content-length") || n.eq_ignore_ascii_case("content-type") {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    if let Ok(v) = HeaderValue::from_str(kind) {
        headers.insert(HeaderName::from_static(ERROR_CODE_HEADER), v);
    }
    if let Ok(v) = HeaderValue::from_str(&sanitize_message_for_header(&message)) {
        headers.insert(HeaderName::from_static(ERROR_MESSAGE_HEADER), v);
    }
    resp
}

/// Map an HTTP status the framework emitted on its own into the
/// canonical exception name Resql uses in the envelope headers. Names
/// mirror the Spring-family patterns already used by
/// `ResqlError::kind()` so downstream DSLs can match on a single
/// stable vocabulary regardless of which layer produced the error.
fn envelope_kind_for_status(s: StatusCode) -> &'static str {
    match s {
        StatusCode::PAYLOAD_TOO_LARGE => "PayloadTooLargeException",
        StatusCode::GATEWAY_TIMEOUT => "RequestTimeoutException",
        StatusCode::REQUEST_TIMEOUT => "RequestTimeoutException",
        StatusCode::METHOD_NOT_ALLOWED => "MethodNotAllowedException",
        StatusCode::NOT_FOUND => "NotFoundException",
        StatusCode::FORBIDDEN => "ForbiddenException",
        StatusCode::UNAUTHORIZED => "UnauthorizedException",
        StatusCode::BAD_REQUEST => "MalformedRequestException",
        s if s.is_server_error() => "InternalError",
        _ => "ResqlRuntimeException",
    }
}

fn derive_envelope_message(parts: &axum::http::response::Parts, bytes: &[u8]) -> String {
    // Prefer the original body when it's a short printable-text hint
    // (that's the shape tower-http's `RequestBodyLimit` produces —
    // "length limit exceeded"). Fall back to the status's reason phrase
    // so 504 with an empty body still carries a human-readable string.
    if !bytes.is_empty() && bytes.len() <= 512 {
        if let Ok(s) = std::str::from_utf8(bytes) {
            let s = s.trim();
            if !s.is_empty() && s.chars().all(|c| c == '\t' || (' '..='~').contains(&c)) {
                return s.to_string();
            }
        }
    }
    parts
        .status
        .canonical_reason()
        .unwrap_or("Error")
        .to_string()
}

/// Same sanitiser as `ResqlError::into_response`'s
/// `sanitize_header_value` — kept private to avoid re-exporting from
/// `error.rs`. Restricts the message to printable ASCII + tab so the
/// header value is HTTP-valid and cannot be used for header splitting.
fn sanitize_message_for_header(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\t' || (' '..='~').contains(&c) {
            out.push(c);
        } else {
            out.push('?');
        }
    }
    out
}

/// Fleet stronghold §5.1: five browser-side defence headers on every
/// response. Resql serves JSON only, so a browser should never render
/// its output — but a proxy misconfiguration (rare, but possible in
/// dev / staging) that serves the JSON as text/html would let a hostile
/// payload get evaluated. These headers close that lane.
///
/// - `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`
///   — a rendered response can neither load nor be framed by anything.
/// - `Strict-Transport-Security: max-age=63072000; includeSubDomains`
///   — pins HTTPS for browsers that speak to Resql directly. Duplicates
///   the reverse-proxy header on well-shaped deployments; adds it when
///   the proxy is missing.
/// - `X-Frame-Options: DENY` — legacy anti-clickjacking; still respected
///   by browsers that don't parse CSP frame-ancestors.
/// - `X-Content-Type-Options: nosniff` — the only header here with a
///   direct payoff on the JSON path; prevents a browser from re-typing
///   the response body away from `application/json`.
/// - `Referrer-Policy: no-referrer` — no referrer header ever leaks a
///   Resql URL to a third party.
async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    // Set-only (not insert-or-append) — a reverse proxy sits ABOVE
    // Resql, so its Set-Cookie / cache-control decisions may override
    // these but should never conflict. entry(...).or_insert(...) would
    // let a handler-set value survive; we don't have any such handler,
    // and preserving the fleet defaults is the more auditable posture.
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    headers.insert(
        HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static("max-age=63072000; includeSubDomains"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

const TRACEPARENT_HEADER: &str = "traceparent";
const TRACE_ID_HEADER: &str = "x-trace-id";
const HEALTHZ_PATH: &str = "/healthz";
const HEALTH_PATH: &str = "/health";

/// Per-request middleware — the single source of Resql's operational
/// log surface. Responsibilities:
///
/// 1. Adopt any inbound W3C `traceparent`, or synthesise one so every
///    request-scoped log line and the response carry a stable id.
/// 2. Open a request span with OpenTelemetry HTTP semantic-convention
///    fields (`http.request.method`, `http.route`, `client.address`,
///    `resql.project`, `trace_id`) that every downstream `info!` /
///    `warn!` / `error!` inherits.
/// 3. After the handler completes: emit a single INFO access-log line
///    with `http.response.status_code` + `duration_ms` (only when
///    `logging.access_log` is on), and echo the trace id back as
///    `X-Trace-Id` so callers can correlate.
///
/// Health probes are excluded from the access log — they'd bury the
/// real traffic.
async fn request_observability(cfg: LoggingConfig, mut req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method_str = req.method().as_str().to_string();
    let uri_path = req.uri().path().to_string();
    let project_str = uri_path
        .split('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();

    let inbound_traceparent = req
        .headers()
        .get(TRACEPARENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let is_health = uri_path == HEALTH_PATH || uri_path == HEALTHZ_PATH;
    let otel_active = crate::observability::otel_active();
    let route_for_log = truncate_for_log(&sanitize_log_value(&uri_path), 512);
    // Export off: the span carries exactly the fields it always had, so log
    // output is unchanged. `trace_id` is recorded right after creation
    // (before any event), once resolved.
    //
    // Export on: a SERVER span whose `http.route` is a template, never the
    // raw path: the saved-query path once `dispatch` has resolved it (see
    // `observability::set_resolved_route`), else the matched axum route or
    // `unmatched`. Health probes get no span: they would dominate the
    // exported data.
    let matched_route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let request_span = if !otel_active {
        tracing::info_span!(
            "http_request",
            http.request.method = %method_str,
            http.route = %route_for_log,
            resql.project = %project_str,
            trace_id = tracing::field::Empty,
        )
    } else if is_health {
        tracing::Span::none()
    } else {
        tracing::info_span!(
            "http_request",
            otel.name = %format!("{method_str} {matched_route}"),
            otel.kind = "server",
            otel.status_code = tracing::field::Empty,
            http.request.method = %method_str,
            http.route = tracing::field::Empty,
            http.response.status_code = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
            resql.project = tracing::field::Empty,
            trace_id = tracing::field::Empty,
            span_id = tracing::field::Empty,
        )
    };
    // Adopt the inbound traceparent verbatim, or generate one. With OTLP
    // export on, the request span is parented on the inbound context and
    // the propagated value is the span's own context (same trace id, this
    // span as parent), so exported spans, logs and `X-Trace-Id` share one
    // trace. Injected back into the request so any header-reading code
    // downstream sees the resolved value.
    let traceparent_value = if otel_active {
        if let Some(tp) = inbound_traceparent.as_deref() {
            crate::observability::set_parent_from_traceparent(&request_span, tp);
        }
        crate::observability::traceparent_of(&request_span)
            .or_else(|| inbound_traceparent.clone())
            .unwrap_or_else(generate_traceparent)
    } else {
        inbound_traceparent
            .clone()
            .unwrap_or_else(generate_traceparent)
    };
    if inbound_traceparent.as_deref() != Some(traceparent_value.as_str()) {
        if let Ok(hv) = HeaderValue::try_from(traceparent_value.as_str()) {
            req.headers_mut()
                .insert(HeaderName::from_static(TRACEPARENT_HEADER), hv);
        }
    }
    let trace_id_str = trace_id_from_traceparent(&traceparent_value)
        .unwrap_or("")
        .to_string();

    // Display (not Debug) so the text formatter renders it unquoted, as before.
    request_span.record("trace_id", tracing::field::display(&trace_id_str));
    // Export on: JSON log lines inside the request carry the OTel span id
    // next to `trace_id`.
    if let Some(span_id) = crate::observability::span_id_of(&request_span) {
        request_span.record("span_id", tracing::field::display(&span_id));
    }
    let span_for_log = request_span.clone();

    let (mut response, resolved_route) = if otel_active {
        crate::observability::with_route_slot(next.run(req).instrument(request_span)).await
    } else {
        (next.run(req).instrument(request_span).await, None)
    };
    if otel_active {
        // `resql.project` only for a resolved saved query, so unknown paths
        // add no free-form attribute values.
        if let Some(project) = resolved_route
            .as_deref()
            .and_then(|r| r.split('/').find(|s| !s.is_empty()))
        {
            span_for_log.record("resql.project", project);
        }
        let route = resolved_route.unwrap_or(matched_route);
        span_for_log.record("http.route", route.as_str());
        span_for_log.record("otel.name", format!("{method_str} {route}"));
        let status = response.status().as_u16();
        span_for_log.record("http.response.status_code", status);
        // 4xx is not a server-side error (OTel HTTP conventions).
        if status >= 500 {
            span_for_log.record("otel.status_code", "ERROR");
            span_for_log.record("error.type", status.to_string());
        }
    }

    // Echo trace id back to caller so they can correlate their logs.
    if !trace_id_str.is_empty() {
        if let Ok(hv) = HeaderValue::try_from(trace_id_str.as_str()) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(TRACE_ID_HEADER), hv);
        }
    }
    // T-17: also emit the resolved W3C `traceparent` on the response so
    // a next-hop service (or a client tracer that already speaks W3C)
    // can chain onto the same trace without having to translate the
    // Resql-specific `X-Trace-Id`. Value is either the inbound one we
    // adopted verbatim, or one we generated — both are validly-shaped
    // W3C traceparents.
    if let Ok(hv) = HeaderValue::try_from(traceparent_value.as_str()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(TRACEPARENT_HEADER), hv);
    }

    // Skip access log for health probes — they'd drown real traffic.
    if cfg.access_log && !is_health {
        let status = response.status().as_u16();
        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        let _guard = span_for_log.enter();
        // 5xx surfaces at WARN — it's operational noise if it's routine;
        // an alert-worthy signal if it isn't. 4xx and 2xx are INFO.
        if status >= 500 {
            tracing::warn!(
                http.request.method = %method_str,
                http.route = %truncate_for_log(&sanitize_log_value(&uri_path), 512),
                http.response.status_code = status,
                duration_ms,
                resql.project = %project_str,
                trace_id = %trace_id_str,
                "http request failed"
            );
        } else {
            tracing::info!(
                http.request.method = %method_str,
                http.route = %truncate_for_log(&sanitize_log_value(&uri_path), 512),
                http.response.status_code = status,
                duration_ms,
                resql.project = %project_str,
                trace_id = %trace_id_str,
                "http request completed"
            );
        }
    }

    response
}

async fn openapi_handler(State(state): State<AppState>) -> Response {
    if !state.config.admin.openapi_public {
        // FN3: 404 by default so an unauth caller can't enumerate every
        // registered SQL endpoint via a single GET. Indistinguishable
        // from a non-mounted endpoint. Operators who need the spec set
        // `admin.openapi_public: true`.
        return StatusCode::NOT_FOUND.into_response();
    }
    Json((*state.openapi).clone()).into_response()
}

/// Build a CORS layer from the `cors.allowed_origins` config.
///
/// - Empty / whitespace → `None`: no layer attached; browsers block
///   cross-origin responses (R2).
/// - `"*"` → wildcard origin, still with narrow methods and headers
///   (R3). No `.allow_credentials(true)` — cookies won't ride along.
/// - Comma-separated hosts → explicit origin allowlist.
///
/// Methods are limited to GET and POST because the router only serves
/// those (R3). Headers are limited to the request headers Resql
/// actually reads.
fn build_cors(allowed: &str) -> Option<CorsLayer> {
    let allowed = allowed.trim();
    if allowed.is_empty() {
        return None;
    }
    let base = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("authorization"),
            HeaderName::from_static(DATASOURCE_HEADER),
            HeaderName::from_static(TRACEPARENT_HEADER),
        ]);
    if allowed == "*" {
        return Some(base.allow_origin(tower_http::cors::Any));
    }
    let origins: Vec<axum::http::HeaderValue> = allowed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| axum::http::HeaderValue::from_str(s).ok())
        .collect();
    if origins.is_empty() {
        // Config was non-empty but nothing parsed. Treat the same as
        // empty — deny — rather than silently opening to `Any`.
        None
    } else {
        Some(base.allow_origin(origins))
    }
}

async fn health_handler(State(state): State<AppState>) -> Json<health::HealthResponse> {
    Json(health::build(state.start))
}

/// `/datasources` response entry.
///
/// Field names remain the Java-canonical camelCase so existing
/// operator dashboards keep parsing, but the values are hardened:
///
/// - `jdbcUrl` is redacted to `<scheme>://<host>[:port]` — path,
///   query, and userinfo (including password) are all stripped.
/// - `username` is always the empty string. The real username stays
///   in server logs at startup only.
///
/// Combined with the `admin.datasources_public` gate (default off,
/// endpoint returns 404), this closes the enumeration lane R5 called
/// out: an unauth caller learns nothing about which hosts, ports, or
/// accounts back the service.
#[derive(Serialize)]
struct DatasourceView {
    name: String,
    #[serde(rename = "jdbcUrl")]
    jdbc_url: String,
    username: String,
    #[serde(rename = "driverClassName")]
    driver_class_name: &'static str,
}

async fn list_datasources(State(state): State<AppState>) -> Response {
    if !state.config.admin.datasources_public {
        // 404 is indistinguishable from a non-mounted endpoint;
        // unauth callers can't confirm the service is Resql.
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut out = Vec::new();
    for name in state.registry.names() {
        let ds_cfg = state.config.datasources.iter().find(|d| d.name == name);
        let driver_class_name = match state.registry.get(&name) {
            Some(crate::db::Pool::Postgres(_)) => "org.postgresql.Driver",
            Some(crate::db::Pool::Sqlite(_)) => "org.sqlite.JDBC",
            // Both mysql:// and mariadb:// URLs land in Pool::MySql and
            // speak the MySQL wire protocol. The Java-compat driver
            // class we report is the official MySQL Connector/J name;
            // Spring configs that want the MariaDB JDBC class can
            // ignore this field (the UI uses it for display only).
            Some(crate::db::Pool::MySql(_)) => "com.mysql.cj.jdbc.Driver",
            None => "",
        };
        let jdbc_url = ds_cfg.map(|d| redact_url(&d.url)).unwrap_or_default();
        out.push(DatasourceView {
            name,
            jdbc_url,
            // Username never returned. Present in the response object
            // as an empty string so JSON structure stays stable for
            // existing consumers that iterate keys.
            username: String::new(),
            driver_class_name,
        });
    }
    Json(out).into_response()
}

/// Redact a datasource URL down to `<scheme>://<host>[:port]`. Any
/// userinfo, path, query, or fragment is dropped. This is the value
/// exposed at `/datasources` — the full URL stays in server startup
/// logs for operator visibility.
///
/// Non-URL-shaped inputs (SQLite's `sqlite::memory:`, `sqlite:file:x`)
/// have the scheme returned with any tail stripped, so an
/// operator can still see the driver without leaking the file path.
pub(crate) fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        // No authority section (e.g. `sqlite::memory:`, `sqlite:foo.db`).
        // Return just the scheme portion up to and including the first
        // colon to hide any file path or in-memory identifier.
        if let Some(colon) = url.find(':') {
            return format!("{}:", &url[..colon]);
        }
        return "***".into();
    };
    let scheme = &url[..scheme_end];
    let rest = &url[scheme_end + 3..];
    // Skip userinfo if present.
    let after_userinfo = match rest.find('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    // Cut at the first path/query/fragment separator.
    let host_port_end = after_userinfo
        .find(['/', '?', '#'])
        .unwrap_or(after_userinfo.len());
    let host_port = &after_userinfo[..host_port_end];
    if host_port.is_empty() {
        format!("{scheme}://")
    } else {
        format!("{scheme}://{host_port}")
    }
}

async fn query_get(
    State(state): State<AppState>,
    Path((project, tail)): Path<(String, String)>,
    Query(params): Query<Map<String, Value>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ResqlError> {
    dispatch(
        state,
        HttpMethod::Get,
        project,
        tail,
        headers,
        Value::Object(params),
    )
    .await
}

async fn query_post(
    State(state): State<AppState>,
    Path((project, tail)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ResqlError> {
    let body_value: Value = if body.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ResqlError::MalformedRequest(format!("invalid JSON body: {e}")))?
    };
    if let Some(stripped) = tail.strip_suffix("/batch") {
        return dispatch_batch(state, project, stripped.to_string(), headers, body_value).await;
    }
    dispatch(state, HttpMethod::Post, project, tail, headers, body_value).await
}

async fn dispatch(
    state: AppState,
    method: HttpMethod,
    project: String,
    tail: String,
    headers: HeaderMap,
    body: Value,
) -> Result<Json<Value>, ResqlError> {
    // Normalise project casing early so query lookup and datasource lookup
    // agree — Java's SavedQuery.getKey lowercased both segments.
    let project_key = project.to_ascii_lowercase();
    let saved = match state.index.get(method, &project_key, &tail) {
        Some(q) => q,
        None => {
            // FN6 (RFC 7231 §7.4.1): if the path IS registered but under
            // a different method, return 405 with an `Allow:` header
            // listing the methods that work. Only fall through to 400
            // QueryNotFound when the path is genuinely unknown.
            let allowed = state.index.allowed_methods_for(&project_key, &tail);
            if !allowed.is_empty() {
                let allow_header = allowed
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ResqlError::MethodNotAllowed {
                    path: truncate_user_input(&format!("/{project}/{tail}")),
                    allowed: allow_header,
                });
            }
            return Err(ResqlError::QueryNotFound(truncate_user_input(&format!(
                "/{project}/{tail}"
            ))));
        }
    };
    // Export on: the request span's `http.route` becomes the saved-query
    // path (bounded by the SQL tree). No-op when export is off.
    crate::observability::set_resolved_route(format!("/{project_key}/{tail}"));
    let ds_name = choose_datasource(&state.config, &project_key, &headers)?;
    let pool = state
        .registry
        .get(&ds_name)
        .ok_or_else(|| ResqlError::UnknownDataSource(ds_name.clone()))?;
    // CLIENT span for the execution when OTLP export is on (`Span::none()`
    // otherwise). Not entered, so log events keep the request span.
    let db_span =
        crate::observability::db_span(pool, &ds_name, &format!("{project_key}/{tail}"), &saved.sql);
    let result = if saved.transactional {
        query::execute_transactional(pool, &saved.sql, &saved.declaration, &body).await
    } else {
        query::execute(pool, &saved.sql, &saved.declaration, &body).await
    };
    crate::observability::record_db_outcome(
        &db_span,
        result.as_ref().ok().map(Vec::len),
        result.as_ref().err().map(ResqlError::kind),
    );
    drop(db_span);
    let rows = result?;
    Ok(Json(json!(rows)))
}

async fn dispatch_batch(
    state: AppState,
    project: String,
    tail: String,
    headers: HeaderMap,
    body: Value,
) -> Result<Json<Value>, ResqlError> {
    let project_key = project.to_ascii_lowercase();
    let saved = state
        .index
        .get(HttpMethod::Post, &project_key, &tail)
        .ok_or_else(|| {
            ResqlError::QueryNotFound(truncate_user_input(&format!("/{project}/{tail}")))
        })?;
    crate::observability::set_resolved_route(format!("/{project_key}/{tail}/batch"));
    let ds_name = choose_datasource(&state.config, &project_key, &headers)?;
    let pool = state
        .registry
        .get(&ds_name)
        .ok_or_else(|| ResqlError::UnknownDataSource(ds_name.clone()))?;

    // `deny_unknown_fields` matches fleet stronghold §2.2 — a caller
    // sending `{"queries":[...],"attacker_field":true}` gets rejected
    // rather than having the extra key silently dropped. Cheap and
    // uniform with the config schema's posture.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BatchBody {
        queries: Vec<Value>,
    }
    let batch: BatchBody = serde_json::from_value(body).map_err(|e| {
        ResqlError::MalformedRequest(format!("batch body must be {{queries: [...]}}: {e}"))
    })?;

    let db_span =
        crate::observability::db_span(pool, &ds_name, &format!("{project_key}/{tail}"), &saved.sql);
    db_span.record("db.operation.batch.size", batch.queries.len() as u64);
    let result = query::execute_batch(pool, &saved.sql, &saved.declaration, batch.queries).await;
    crate::observability::record_db_outcome(
        &db_span,
        result
            .as_ref()
            .ok()
            .map(|sets| sets.iter().map(Vec::len).sum()),
        result.as_ref().err().map(ResqlError::kind),
    );
    drop(db_span);
    let results = result?;
    let all: Vec<Value> = results.into_iter().map(|rows| json!(rows)).collect();
    Ok(Json(Value::Array(all)))
}

/// Choose the datasource for a given project + headers.
///
/// - Default (no `X-Datasource`): project's own datasource via
///   `datasource_for_project`.
/// - `X-Datasource: <name>` when `allow_datasource_header` is true and
///   the (project, name) pair is in `datasource_header_allowlist`:
///   returns that name and logs the override at INFO.
/// - `X-Datasource` present but `allow_datasource_header` is false:
///   silently ignored (header routing is off).
/// - `X-Datasource: <name>` present but (project, name) is not in the
///   allowlist: returns `Forbidden` (R1). This includes projects that
///   have no allowlist entry at all — every override is opt-in.
fn choose_datasource(
    cfg: &Config,
    project: &str,
    headers: &HeaderMap,
) -> Result<String, ResqlError> {
    if cfg.allow_datasource_header {
        if let Some(v) = headers.get(DATASOURCE_HEADER).and_then(|h| h.to_str().ok()) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                let allowed = cfg
                    .datasource_header_allowlist
                    .get(project)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                // FN-LOG-1: `trimmed` is a caller-supplied header
                // value. HTTP already restricts it to visible ASCII,
                // but nothing bounds its length — a very long value
                // would balloon the log line and (below) the error's
                // header echo. Cap for logging + downstream reuse.
                let logged_override = truncate_for_log(trimmed, 256);
                if !allowed.iter().any(|d| d == trimmed) {
                    tracing::warn!(
                        resql.project = %project,
                        override_to = %logged_override,
                        "X-Datasource override rejected: not in allowlist"
                    );
                    return Err(ResqlError::ForbiddenDatasourceOverride {
                        project: project.to_string(),
                        requested: trimmed.to_string(),
                    });
                }
                tracing::info!(
                    resql.project = %project,
                    override_to = %logged_override,
                    "X-Datasource override accepted"
                );
                return Ok(trimmed.to_string());
            }
        }
    }
    Ok(cfg.datasource_for_project(project))
}

/// Convenience: build a full AppState from a Config, connecting all
/// datasources and loading the SQL directory.
pub async fn init(config: Config) -> Result<AppState, ResqlError> {
    let index = crate::loader::load_dir(&config.sql_dir)?;
    // Propagate the request timeout to every Postgres connection as
    // its `statement_timeout` — so a query that outlives the HTTP
    // middleware cancellation is still killed at the server side and
    // the pool slot is returned. See `db::connect` for the SET call.
    let registry = DatasourceRegistry::connect_all_with_timeout(
        &config.datasources,
        Some(config.server.request_timeout_seconds),
    )
    .await?;
    let spec = openapi::build_spec(&index, &config.openapi);
    let bearer = Arc::new(config.resolved_inter_service_token());
    let rate_limiter = if config.rate_limit.requests_per_second > 0 {
        Some(RateLimiter::new(
            config.rate_limit.requests_per_second,
            config.effective_rate_limit_burst(),
        ))
    } else {
        None
    };
    Ok(AppState {
        config: Arc::new(config),
        index: Arc::new(index),
        registry: Arc::new(registry),
        start: StartTime::now(),
        openapi: Arc::new(spec),
        inter_service_token: bearer,
        rate_limiter: Arc::new(rate_limiter),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn redact_url_strips_userinfo_path_and_query() {
        // Full userinfo + database path + query string all gone.
        assert_eq!(
            redact_url("postgres://user:secret@host:5432/db?sslmode=require"),
            "postgres://host:5432"
        );
    }

    #[test]
    fn redact_url_keeps_host_and_port_when_no_userinfo() {
        assert_eq!(
            redact_url("postgresql://internal-db.corp:5432/users_prod"),
            "postgresql://internal-db.corp:5432"
        );
    }

    #[test]
    fn redact_url_hides_sqlite_paths() {
        // SQLite URLs don't have a `//authority`; redact to just the
        // scheme so the file path (which may include usernames or leak
        // deployment topology) is not disclosed.
        assert_eq!(redact_url("sqlite::memory:"), "sqlite:");
        assert_eq!(redact_url("sqlite:/var/lib/resql/audit.db"), "sqlite:");
    }

    #[test]
    fn redact_url_handles_mysql_and_mariadb_urls() {
        // Same guarantee as Postgres: strip userinfo, path, and query.
        // Guard the MySQL/MariaDB schemes against future redact
        // changes that might only recognise the two older schemes.
        assert_eq!(
            redact_url("mysql://resql:supersecret@db.internal:3306/app?charset=utf8"),
            "mysql://db.internal:3306"
        );
        assert_eq!(
            redact_url("mariadb://admin:p@maria.corp:3306/users"),
            "mariadb://maria.corp:3306"
        );
    }

    // All server tests below bind to loopback to satisfy fleet §3.1's
    // boot-refuse check; the check itself is exercised in `config::tests`.
    const LOOPBACK_HEADER: &str = "sql_dir: ./sql\nserver:\n  bind: \"127.0.0.1:8080\"\n";

    #[test]
    fn choose_datasource_defaults_to_project_name() {
        let cfg = Config::from_yaml_str(LOOPBACK_HEADER).unwrap();
        let headers = HeaderMap::new();
        assert_eq!(choose_datasource(&cfg, "crm", &headers).unwrap(), "crm");
    }

    #[test]
    fn choose_datasource_respects_header_when_allowed_and_allowlisted() {
        let yaml = r#"
sql_dir: ./sql
server:
  bind: "127.0.0.1:8080"
allow_datasource_header: true
datasource_header_allowlist:
  crm: [other]
datasources:
  - name: other
    url: "sqlite::memory:"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(DATASOURCE_HEADER, HeaderValue::from_static("other"));
        assert_eq!(choose_datasource(&cfg, "crm", &headers).unwrap(), "other");
    }

    #[test]
    fn choose_datasource_ignores_header_when_disabled() {
        let cfg = Config::from_yaml_str(&format!(
            "{LOOPBACK_HEADER}allow_datasource_header: false\n"
        ))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(DATASOURCE_HEADER, HeaderValue::from_static("other"));
        assert_eq!(choose_datasource(&cfg, "crm", &headers).unwrap(), "crm");
    }

    #[test]
    fn choose_datasource_map_takes_precedence_over_default() {
        let yaml = r#"
sql_dir: ./sql
server:
  bind: "127.0.0.1:8080"
project_datasource_map:
  crm: db1
datasources:
  - name: db1
    url: "sqlite::memory:"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let headers = HeaderMap::new();
        assert_eq!(choose_datasource(&cfg, "crm", &headers).unwrap(), "db1");
    }

    #[test]
    fn choose_datasource_header_rejected_without_allowlist_entry() {
        // allow_datasource_header is on, but no allowlist entry for the
        // project → header override is refused (R1).
        let yaml = r#"
sql_dir: ./sql
server:
  bind: "127.0.0.1:8080"
allow_datasource_header: true
datasources:
  - name: db1
    url: "sqlite::memory:"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(DATASOURCE_HEADER, HeaderValue::from_static("db1"));
        let err = choose_datasource(&cfg, "crm", &headers).unwrap_err();
        assert!(
            matches!(err, ResqlError::ForbiddenDatasourceOverride { .. }),
            "err = {err:?}"
        );
    }

    #[test]
    fn choose_datasource_header_rejected_when_not_in_allowlist() {
        let yaml = r#"
sql_dir: ./sql
server:
  bind: "127.0.0.1:8080"
allow_datasource_header: true
datasource_header_allowlist:
  crm: [db1]
datasources:
  - name: db1
    url: "sqlite::memory:"
  - name: db2
    url: "sqlite::memory:"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(DATASOURCE_HEADER, HeaderValue::from_static("db2"));
        let err = choose_datasource(&cfg, "crm", &headers).unwrap_err();
        assert!(matches!(
            err,
            ResqlError::ForbiddenDatasourceOverride { .. }
        ));
    }

    #[test]
    fn choose_datasource_empty_header_falls_through() {
        // A blank/whitespace-only header value is treated as absent.
        let yaml = r#"
sql_dir: ./sql
server:
  bind: "127.0.0.1:8080"
allow_datasource_header: true
datasources:
  - name: crm
    url: "sqlite::memory:"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(DATASOURCE_HEADER, HeaderValue::from_static("   "));
        assert_eq!(choose_datasource(&cfg, "crm", &headers).unwrap(), "crm");
    }

    #[test]
    fn build_cors_none_when_empty() {
        assert!(build_cors("").is_none());
        assert!(build_cors("   ").is_none());
    }

    #[test]
    fn build_cors_some_when_wildcard() {
        assert!(build_cors("*").is_some());
    }

    #[test]
    fn build_cors_some_when_specific_origin() {
        assert!(build_cors("https://app.example.com").is_some());
    }
}
