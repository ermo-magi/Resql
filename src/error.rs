use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use std::path::PathBuf;
use thiserror::Error;

use crate::logging::{sanitize_log_value, truncate_for_log};

/// Response headers that carry the error envelope on non-2xx responses.
/// See `IntoResponse for ResqlError` (issue #25) — the response body itself
/// is always the empty JSON array `[]` so a naive DSL check like
/// `body.length > 0` cannot silently fail-open into "empty result" when
/// the query actually errored.
pub const ERROR_CODE_HEADER: &str = "x-resql-error-code";
pub const ERROR_MESSAGE_HEADER: &str = "x-resql-error-message";

/// Maximum length of the WARN/ERROR log line's message text and of
/// the outgoing `X-Resql-Error-Message` header value. Above this the
/// message is truncated with a marker so a caller sending a 100 KB
/// parameter name cannot fill the log store or reject-on-oversize the
/// header. See FN-LOG-1 / FN-LOG-2. 1 KB is enough for every real
/// error the codebase produces (measured — the longest under normal
/// use is ~200 chars).
const LOG_MESSAGE_MAX_BYTES: usize = 1024;
const HEADER_MESSAGE_MAX_BYTES: usize = 1024;

#[derive(Debug, Error)]
pub enum ResqlError {
    #[error("Saved query '{0}' does not exist")]
    QueryNotFound(String),

    #[error("Specified dataSourceName name: '{0}' is unknown to the service")]
    UnknownDataSource(String),

    #[error("No value supplied for the SQL parameter '{0}': No value registered for key '{0}'")]
    MissingParameter(String),

    #[error("Unexpected parameter '{0}' not declared for this endpoint")]
    UnknownParameter(String),

    #[error("Parameter '{name}' has the wrong type: expected {expected}, got {actual}")]
    InvalidParameterType {
        name: String,
        expected: &'static str,
        actual: String,
    },

    #[error("Parameter '{name}' value {value} is not in the allowed set {allowed:?}")]
    InvalidParameterValue {
        name: String,
        value: String,
        allowed: Vec<String>,
    },

    #[error("Invalid query file '{path}': {reason}")]
    InvalidQuery { path: PathBuf, reason: String },

    #[error("Invalid declaration in '{path}': {reason}")]
    InvalidDeclaration { path: PathBuf, reason: String },

    #[error("Invalid SQL directory '{path}': {reason}")]
    InvalidDirectory { path: PathBuf, reason: String },

    #[error("SQL execution failed: {0}")]
    SqlExecution(String),

    #[error("Request body too large")]
    BodyTooLarge,

    #[error("Malformed request body: {0}")]
    MalformedRequest(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error(
        "X-Datasource '{requested}' is not permitted for project '{project}' (not in allowlist)"
    )]
    ForbiddenDatasourceOverride { project: String, requested: String },

    /// Generic batch failure — reveals only the failing statement's
    /// position and the batch size, never the underlying Postgres /
    /// SQLite error text. The full detail is emitted at WARN in the
    /// server log so operators can debug without the caller seeing
    /// which constraint / table / column tripped.
    #[error("Batch failed at statement {index} of {total}, rolled back")]
    BatchStatementFailed { index: usize, total: usize },

    /// The inter-service bearer gate (`SecurityConfig`) is enabled and
    /// the request either omitted `Authorization: Bearer …` or
    /// presented a token that failed constant-time comparison. The
    /// caller-visible message is deliberately generic (no "invalid
    /// token" vs "missing header" distinction) so a probe cannot be
    /// used as an oracle for whether a specific header shape counts as
    /// "sent a token". F-RES-3.
    #[error("Authentication required")]
    Unauthorized,

    /// A saved query exists at the requested path but only under a
    /// different HTTP method. Returned by the dispatcher as `405 Method
    /// Not Allowed` with an `Allow:` header listing the supported
    /// methods, per RFC 7231 §7.4.1. Previously such requests returned
    /// 400 `Saved query '<path>' does not exist`, which is misleading
    /// — the path IS registered, just under a different method (FN6).
    #[error("Method not allowed for '{path}' — try {allowed}")]
    MethodNotAllowed { path: String, allowed: String },

    /// R8: rate-limit bucket exhausted. Caller-visible message tells
    /// them when to retry; the same value goes into the `Retry-After`
    /// response header (see server::rate_limit_middleware).
    #[error("Too many requests, retry after {retry_after_secs} seconds")]
    TooManyRequests { retry_after_secs: u64 },
}

impl ResqlError {
    /// The `X-Resql-Error-Code` value for this error.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            ResqlError::QueryNotFound(_) => "ResqlRuntimeException",
            ResqlError::UnknownDataSource(_) => "UnknownDataSourceNameException",
            ResqlError::MissingParameter(_) => "InvalidDataAccessApiUsageException",
            ResqlError::UnknownParameter(_) => "UnknownParameterException",
            ResqlError::InvalidParameterType { .. } => "InvalidParameterTypeException",
            ResqlError::InvalidParameterValue { .. } => "InvalidParameterValueException",
            ResqlError::InvalidQuery { .. } => "InvalidQueryException",
            ResqlError::InvalidDeclaration { .. } => "InvalidDeclarationException",
            ResqlError::InvalidDirectory { .. } => "InvalidDirectoryException",
            ResqlError::SqlExecution(_) => "BadSqlGrammarException",
            ResqlError::BodyTooLarge => "PayloadTooLargeException",
            ResqlError::MalformedRequest(_) => "MalformedRequestException",
            ResqlError::Internal(_) => "InternalError",
            ResqlError::ForbiddenDatasourceOverride { .. } => {
                "ForbiddenDatasourceOverrideException"
            }
            ResqlError::BatchStatementFailed { .. } => "BadSqlGrammarException",
            ResqlError::Unauthorized => "UnauthorizedException",
            ResqlError::MethodNotAllowed { .. } => "MethodNotAllowedException",
            ResqlError::TooManyRequests { .. } => "TooManyRequestsException",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            ResqlError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ResqlError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ResqlError::ForbiddenDatasourceOverride { .. } => StatusCode::FORBIDDEN,
            ResqlError::Unauthorized => StatusCode::UNAUTHORIZED,
            ResqlError::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            ResqlError::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl IntoResponse for ResqlError {
    fn into_response(self) -> Response {
        // Emit a structured log line for every error we return so
        // operators see the failure alongside the response. Field
        // names align with the OpenTelemetry HTTP semantic-conventions
        // used by the request middleware; the source-chain toggle is
        // handled by the middleware / handler that has config access
        // (see `logging.print_stack_trace`).
        let status = self.status();
        let status_code = status.as_u16();
        let kind = self.kind();
        let message = self.to_string();
        // FN-LOG-1 / FN-LOG-2: several error variants interpolate a
        // caller-supplied string (parameter name, saved-query path,
        // datasource header) into `message` via Display. Emitting that
        // through the log message template (`"{message}"`) would put
        // raw CRLF bytes on the wire and place no cap on line length —
        // a 100 KB JSON key would produce a 100 KB WARN. Strip CR/LF
        // and clip to a fixed budget before it lands in the log.
        let safe_message = truncate_for_log(&sanitize_log_value(&message), LOG_MESSAGE_MAX_BYTES);
        if status_code >= 500 {
            tracing::error!(
                error.kind = %kind,
                http.response.status_code = status_code,
                "{safe_message}"
            );
        } else {
            tracing::warn!(
                error.kind = %kind,
                http.response.status_code = status_code,
                "{safe_message}"
            );
        }
        // Body is always the empty JSON array — a naive DSL check like
        // `body.length > 0` sees 0 rows on any error, so a DB failure
        // no longer routes to a "not_found" branch by silently having
        // an object body whose `.length` is `undefined`. Structured
        // error info moves to the two response headers below. See #25.
        let allow_hint = match &self {
            ResqlError::MethodNotAllowed { allowed, .. } => Some(allowed.clone()),
            _ => None,
        };
        let mut resp = (status, Json(Value::Array(Vec::new()))).into_response();
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(kind) {
            headers.insert(HeaderName::from_static(ERROR_CODE_HEADER), v);
        }
        if let Ok(v) = HeaderValue::from_str(&sanitize_header_value(&message)) {
            headers.insert(HeaderName::from_static(ERROR_MESSAGE_HEADER), v);
        }
        // RFC 7231 §7.4.1: a 405 response MUST include an `Allow:`
        // header listing the methods the resource does support.
        if let Some(allow) = allow_hint {
            if let Ok(v) = HeaderValue::from_str(&allow) {
                headers.insert(axum::http::header::ALLOW, v);
            }
        }
        resp
    }
}

/// Restrict a free-form message to characters valid in an HTTP header
/// value: printable ASCII (0x20–0x7E) plus horizontal tab. CR/LF are
/// dropped (would inject a header split); everything else is replaced
/// with `?`. Also caps at `HEADER_MESSAGE_MAX_BYTES` so a 100 KB
/// parameter name from a caller can't produce a header value the peer
/// (or an intermediate proxy) will reject on size. See FN-LOG-1. The
/// un-sanitised, un-truncated message is still available in the server
/// log line emitted just above.
fn sanitize_header_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(HEADER_MESSAGE_MAX_BYTES));
    for c in s.chars() {
        if out.len() >= HEADER_MESSAGE_MAX_BYTES {
            out.push_str("...");
            break;
        }
        if c == '\t' || (' '..='~').contains(&c) {
            out.push(c);
        } else {
            out.push('?');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_not_found_matches_java_message() {
        let e = ResqlError::QueryNotFound("/foo/bar".into());
        assert_eq!(e.to_string(), "Saved query '/foo/bar' does not exist");
        assert_eq!(e.kind(), "ResqlRuntimeException");
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unknown_datasource_matches_java_message() {
        let e = ResqlError::UnknownDataSource("orders".into());
        assert_eq!(
            e.to_string(),
            "Specified dataSourceName name: 'orders' is unknown to the service"
        );
        assert_eq!(e.kind(), "UnknownDataSourceNameException");
    }

    #[test]
    fn missing_parameter_message_shape() {
        let e = ResqlError::MissingParameter("login".into());
        assert!(e.to_string().contains("'login'"));
        assert_eq!(e.kind(), "InvalidDataAccessApiUsageException");
    }

    #[test]
    fn body_too_large_is_413() {
        assert_eq!(
            ResqlError::BodyTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn sanitize_header_value_strips_crlf_and_non_ascii() {
        // Newlines would header-split; non-printable becomes '?'.
        assert_eq!(sanitize_header_value("a\r\nb"), "a??b");
        assert_eq!(sanitize_header_value("café"), "caf?");
        // Tabs and printable ASCII survive.
        assert_eq!(sanitize_header_value("a\tb c"), "a\tb c");
    }

    #[test]
    fn sanitize_header_value_caps_at_max_bytes() {
        // FN-LOG-1: a caller sending a 100 KB parameter name must not
        // produce a 100 KB header value. Everything past the budget is
        // replaced with the trailing "..." marker so a peer / proxy
        // that enforces its own header-size cap doesn't reject the
        // whole response.
        let huge = "A".repeat(4096);
        let out = sanitize_header_value(&huge);
        assert!(out.len() <= HEADER_MESSAGE_MAX_BYTES + 3);
        assert!(out.starts_with(&"A".repeat(HEADER_MESSAGE_MAX_BYTES)));
        assert!(out.ends_with("..."));
    }

    #[tokio::test]
    async fn into_response_body_is_empty_array_and_headers_carry_kind() {
        use http_body_util::BodyExt;
        let resp = ResqlError::QueryNotFound("/foo/bar".into()).into_response();
        let status = resp.status();
        let code = resp
            .headers()
            .get(ERROR_CODE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let message = resp
            .headers()
            .get(ERROR_MESSAGE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "ResqlRuntimeException");
        assert!(message.contains("/foo/bar"), "message = {message}");
        // The DSL-safety invariant: naive `body.length > 0` must see 0.
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "[]");
    }
}
