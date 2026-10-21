//! Fixed public classifications. Never retain a driver, input, path or source cause.
use assignment_manager_application::error::ApplicationError;
use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebError {
    Unauthorized,
    Forbidden,
    InvalidCsrf,
    RateLimited,
    SetupRequired,
    AccountExists,
    InvalidQuery,
    InvalidRequest,
    InvalidHost,
    CrossOrigin,
    NotFound,
    MethodNotAllowed,
    UnsupportedMediaType,
    PayloadTooLarge,
    Busy,
    Unavailable,
    Internal,
    VaultUnavailable,
    ReauthRequired,
    ReauthFailed,
}

impl WebError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unauthorized => "authentication_failed",
            Self::Forbidden => "forbidden",
            Self::InvalidCsrf => "invalid_csrf",
            Self::RateLimited => "rate_limited",
            Self::SetupRequired => "bootstrap_required",
            Self::AccountExists => "account_exists",
            Self::InvalidQuery => "invalid_query",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidHost => "invalid_host",
            Self::CrossOrigin => "cross_origin",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::PayloadTooLarge => "payload_too_large",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal_error",
            Self::VaultUnavailable => "vault_unavailable",
            Self::ReauthRequired => "reauth_required",
            Self::ReauthFailed => "reauth_failed",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::InvalidCsrf | Self::ReauthRequired | Self::ReauthFailed => {
                StatusCode::FORBIDDEN
            }
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::SetupRequired | Self::AccountExists => StatusCode::CONFLICT,
            Self::InvalidQuery | Self::InvalidRequest | Self::InvalidHost => {
                StatusCode::BAD_REQUEST
            }
            Self::CrossOrigin => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Busy | Self::Unavailable | Self::VaultUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<ApplicationError> for WebError {
    fn from(value: ApplicationError) -> Self {
        match value {
            ApplicationError::InvalidQuery => Self::InvalidQuery,
            ApplicationError::InvalidAssignmentId => Self::InvalidRequest,
            ApplicationError::InvalidMoodleFeed | ApplicationError::InvalidCredentials => {
                Self::InvalidRequest
            }
            ApplicationError::CredentialUnavailable
            | ApplicationError::CredentialReadFailed
            | ApplicationError::CredentialWriteFailed
            | ApplicationError::CredentialDeleteFailed => Self::VaultUnavailable,
            ApplicationError::NotFound => Self::NotFound,
            ApplicationError::Busy => Self::Busy,
            ApplicationError::Unavailable => Self::Unavailable,
            _ => Self::Internal,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorCode,
}
#[derive(Serialize)]
struct ErrorCode {
    code: &'static str,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status(),
            Json(ErrorBody {
                error: ErrorCode { code: self.code() },
            }),
        )
            .into_response();
        if self.status() == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        }
        if self == Self::RateLimited {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
        }
        response
    }
}
impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl Error for WebError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn application_errors_have_fixed_http_mappings_without_causes() {
        for (input, output, status) in [
            (ApplicationError::InvalidQuery, WebError::InvalidQuery, 400),
            (ApplicationError::Busy, WebError::Busy, 503),
            (ApplicationError::Unavailable, WebError::Unavailable, 503),
            (ApplicationError::Storage, WebError::Internal, 500),
            (ApplicationError::DatabaseStartup, WebError::Internal, 500),
        ] {
            let error = WebError::from(input);
            assert_eq!(error, output);
            assert_eq!(error.status().as_u16(), status);
            assert!(error.source().is_none());
            assert_eq!(error.to_string(), error.code());
        }
    }
}
