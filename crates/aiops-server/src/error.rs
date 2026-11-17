//! One error type and one JSON envelope for every error the collector emits, including
//! those produced by middleware (rust-mastery chapter 12, section 6).

use axum::Json;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("missing, malformed, unknown, expired or revoked credential")]
    Unauthenticated,
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("not found")]
    NotFound,
    #[error("the request is invalid: {0}")]
    BadRequest(&'static str),
    #[error("the batch is not valid JSON of the schema's envelope shape")]
    MalformedBatch,
    #[error("unsupported schema_version (this collector accepts 1)")]
    UnsupportedSchema,
    #[error("the batch source is invalid: {0}")]
    InvalidSource(&'static str),
    #[error("the batch has more events than the collector accepts ({0})")]
    TooManyEvents(usize),
    /// The store could not keep one event's text; nothing was written. A sender splits the
    /// batch to isolate the event (review N1).
    #[error("the batch holds text the store cannot keep; split it and resend")]
    UnstorableText,
    #[error("the batch's source instance belongs to another ingest key")]
    InstanceConflict,
    #[error("rate limit exceeded")]
    RateLimited { retry_after_secs: u64 },
    #[error("the collector is at capacity; retry later")]
    Overloaded,
    #[error("the store is unavailable; retry later")]
    StoreUnavailable,
    #[error("the audit log is unavailable, so the request was refused")]
    AuditUnavailable,
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicError {
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize)]
struct Envelope<'a> {
    error: EnvelopeBody<'a>,
}

#[derive(Serialize)]
struct EnvelopeBody<'a> {
    code: &'a str,
    message: &'a str,
    request_id: Option<&'a str>,
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_)
            | Self::MalformedBatch
            | Self::UnsupportedSchema
            | Self::InvalidSource(_)
            | Self::UnstorableText => StatusCode::BAD_REQUEST,
            Self::TooManyEvents(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InstanceConflict => StatusCode::CONFLICT,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Overloaded | Self::StoreUnavailable | Self::AuditUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::MalformedBatch => "malformed_batch",
            Self::UnsupportedSchema => "unsupported_schema_version",
            Self::InvalidSource(_) => "invalid_source",
            Self::TooManyEvents(_) => "too_many_events",
            Self::UnstorableText => "unstorable_text",
            Self::InstanceConflict => "instance_owned_by_another_key",
            Self::RateLimited { .. } => "rate_limited",
            Self::Overloaded => "overloaded",
            Self::StoreUnavailable => "store_unavailable",
            Self::AuditUnavailable => "audit_unavailable",
            Self::Internal(_) => "internal",
        }
    }

    fn log(&self) {
        match self {
            Self::Internal(e) => tracing::error!(error = format!("{e:#}"), "internal error"),
            Self::StoreUnavailable | Self::AuditUnavailable | Self::Overloaded => {
                tracing::warn!(code = self.code(), "request refused for capacity");
            }
            _ => tracing::debug!(code = self.code(), "client error"),
        }
    }
}

impl From<aiops_store::StoreError> for ApiError {
    fn from(e: aiops_store::StoreError) -> Self {
        match e {
            aiops_store::StoreError::InstanceOwnedByAnotherKey => return Self::InstanceConflict,
            aiops_store::StoreError::Unstorable(_) => {
                tracing::warn!(error = %e, "a batch held text the store cannot keep");
                return Self::UnstorableText;
            }
            _ => {}
        }
        if e.is_retryable() {
            tracing::warn!(error = %e, cause = ?std::error::Error::source(&e).map(ToString::to_string), "store unavailable");
            Self::StoreUnavailable
        } else {
            Self::Internal(anyhow::Error::new(e))
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.log();
        let public = PublicError {
            code: self.code(),
            message: self.to_string(),
        };
        let mut response = render(self.status(), &public, None);
        let retry_after = match self {
            Self::RateLimited { retry_after_secs } => Some(retry_after_secs),
            Self::Overloaded | Self::StoreUnavailable | Self::AuditUnavailable => Some(1),
            _ => None,
        };
        if let Some(secs) = retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response.extensions_mut().insert(public);
        response
    }
}

fn render(status: StatusCode, public: &PublicError, request_id: Option<&str>) -> Response {
    let body = Envelope {
        error: EnvelopeBody {
            code: public.code,
            message: &public.message,
            request_id,
        },
    };
    (status, Json(body)).into_response()
}

fn public_for_status(status: StatusCode) -> PublicError {
    let (code, message) = match status {
        StatusCode::PAYLOAD_TOO_LARGE => (
            "payload_too_large",
            "the request body exceeds the configured limit",
        ),
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => (
            "timeout",
            "the request did not finish within the collector's deadline",
        ),
        StatusCode::METHOD_NOT_ALLOWED => ("method_not_allowed", "method not allowed"),
        StatusCode::NOT_FOUND => ("not_found", "not found"),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            ("unsupported_media_type", "expected application/json")
        }
        s if s.is_client_error() => ("bad_request", "the request is invalid"),
        _ => ("internal", "internal error"),
    };
    PublicError {
        code,
        message: message.to_owned(),
    }
}

/// Re-renders every error response with the request ID, normalizing errors produced by
/// layers that do not speak the envelope (413 from the body limit, the timeout, 405).
pub async fn error_envelope(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let public = match response.extensions().get::<PublicError>() {
        Some(public) => public.clone(),
        None if is_json(&response) => return response,
        None => public_for_status(status),
    };
    let (mut parts, _) = response.into_parts();
    let fresh = render(status, &public, request_id.as_deref());
    let (fresh_parts, body) = fresh.into_parts();
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    drop(fresh_parts);
    Response::from_parts(parts, body)
}

fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

/// Drops a client-supplied request ID that is not a short token, so it cannot flood logs.
pub async fn sanitize_request_id(mut request: Request, next: Next) -> Response {
    let bad = request.headers().get(REQUEST_ID_HEADER).is_some_and(|v| {
        v.len() > 64
            || !v
                .as_bytes()
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    });
    if bad {
        request.headers_mut().remove(REQUEST_ID_HEADER);
    }
    next.run(request).await
}
