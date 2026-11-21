//! HTTP error mapping from domain, origin, engine, and cache failures.
//!
//! Internal host paths and raw I/O details are intentionally withheld from client
//! responses to prevent information disclosure.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::cache::CacheError;
use crate::domain::DomainError;
use crate::engine::EngineError;
use crate::origin::OriginError;

/// Transport-level error converted into an HTTP status code and plain-text message.
#[derive(Debug, Clone)]
pub enum HttpError {
    /// Request query or parameters failed validation (`400 Bad Request`).
    BadRequest(String),
    /// Missing or invalid authentication credentials (`401 Unauthorized`).
    Unauthorized,
    /// Access to the requested resource is denied by security boundaries (`403 Forbidden`).
    Forbidden(String),
    /// Requested asset could not be found (`404 Not Found`).
    NotFound,
    /// Source payload exceeded configured size limits (`413 Payload Too Large`).
    PayloadTooLarge(String),
    /// Input or requested output image format is not supported (`415 Unsupported Media Type`).
    UnsupportedMediaType(String),
    /// Image could not be decoded or exceeded dimension/pixel limits (`422 Unprocessable Entity`).
    UnprocessableEntity(String),
    /// Requested operation is not yet implemented (`501 Not Implemented`).
    NotImplemented(String),
    /// Upstream origin failed or returned an unexpected response (`502 Bad Gateway`).
    BadGateway(String),
    /// Upstream origin timed out (`504 Gateway Timeout`).
    GatewayTimeout,
    /// Unexpected internal processing or storage error (`500 Internal Server Error`).
    Internal,
}

impl From<DomainError> for HttpError {
    fn from(err: DomainError) -> Self {
        Self::BadRequest(err.to_string())
    }
}

impl From<OriginError> for HttpError {
    fn from(err: OriginError) -> Self {
        match err {
            OriginError::InvalidPath(reason) => Self::BadRequest(reason),
            OriginError::PathEscapesRoot => {
                Self::Forbidden("requested path is outside the allowed origin root".to_owned())
            }
            OriginError::HostNotAllowed | OriginError::ForbiddenIpAddress => {
                Self::Forbidden(err.to_string())
            }
            // Treat directories, non-regular files, and upstream 404s as 404.
            OriginError::NotFound
            | OriginError::NotRegularFile
            | OriginError::UpstreamStatus(404) => Self::NotFound,
            OriginError::TooManyRedirects { .. } | OriginError::UpstreamStatus(_) => {
                Self::BadGateway(err.to_string())
            }
            OriginError::Timeout => Self::GatewayTimeout,
            OriginError::PayloadTooLarge { size, max } => Self::PayloadTooLarge(format!(
                "source image ({size} bytes) exceeds limit of {max} bytes"
            )),
            OriginError::InvalidRoot(_) | OriginError::Io(_) => Self::Internal,
        }
    }
}

impl From<EngineError> for HttpError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::InputTooLarge { size, max } => Self::PayloadTooLarge(format!(
                "source image ({size} bytes) exceeds limit of {max} bytes"
            )),
            EngineError::UnsupportedInputFormat | EngineError::UnsupportedOutputFormat(_) => {
                Self::UnsupportedMediaType(err.to_string())
            }
            EngineError::SourceDimensionLimitExceeded { .. }
            | EngineError::DecodedPixelLimitExceeded { .. }
            | EngineError::DecodeFailed(_) => Self::UnprocessableEntity(err.to_string()),
            EngineError::EncodeFailed(_) => Self::Internal,
        }
    }
}

impl From<CacheError> for HttpError {
    fn from(_err: CacheError) -> Self {
        Self::Internal
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorised".to_owned()),
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            Self::NotFound => (StatusCode::NOT_FOUND, "image not found".to_owned()),
            Self::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg),
            Self::UnsupportedMediaType(msg) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, msg),
            Self::UnprocessableEntity(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg),
            Self::NotImplemented(msg) => (StatusCode::NOT_IMPLEMENTED, msg),
            Self::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg),
            Self::GatewayTimeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "upstream origin request timed out".to_owned(),
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal image processing error".to_owned(),
            ),
        };

        (status, message).into_response()
    }
}
