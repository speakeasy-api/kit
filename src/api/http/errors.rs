use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::api::service::ServiceError;

pub const PROBLEM_MEDIA_TYPE: &str = "application/problem+json";
const TYPE_ROOT: &str = "https://kit.dev/problems/";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InvalidParameter {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub problem_type: Box<str>,
    pub title: &'static str,
    pub status: u16,
    pub detail: Box<str>,
    pub instance: Box<str>,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub invalid_parameters: Vec<InvalidParameter>,
}

impl ProblemDetails {
    pub fn unauthenticated(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "Authentication required",
            "Authentication is required for this resource.",
            "unauthenticated",
            instance,
        )
    }

    pub fn not_found(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "Resource not found",
            "The requested resource was not found.",
            "not_found",
            instance,
        )
    }

    pub fn invalid(
        instance: impl Into<String>,
        name: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let mut problem = Self::new(
            StatusCode::BAD_REQUEST,
            "Invalid request",
            "The request did not satisfy the API contract.",
            "invalid_request",
            instance,
        );
        problem.invalid_parameters.push(InvalidParameter {
            name: name.into(),
            reason: reason.into(),
        });
        problem
    }

    pub fn missing_idempotency_key(instance: impl Into<String>) -> Self {
        Self::invalid(
            instance,
            "Idempotency-Key",
            "A valid Idempotency-Key header is required for mutations.",
        )
    }

    pub fn unsupported_media_type(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported media type",
            "Request bodies must use application/json.",
            "unsupported_media_type",
            instance,
        )
    }

    pub fn payload_too_large(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Payload too large",
            "The request body exceeds the configured limit.",
            "payload_too_large",
            instance,
        )
    }

    pub fn timeout(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "Request timed out",
            "The request exceeded the configured processing deadline.",
            "request_timeout",
            instance,
        )
    }

    pub fn method_not_allowed(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "Method not allowed",
            "The HTTP method is not supported for this resource.",
            "method_not_allowed",
            instance,
        )
    }

    pub fn service(error: ServiceError, instance: impl Into<String>) -> Self {
        let instance = instance.into();
        match error {
            ServiceError::NotFound | ServiceError::Authentication(_) => Self::not_found(instance),
            ServiceError::MissingIdempotencyKey => Self::missing_idempotency_key(instance),
            ServiceError::Conflict(_) => Self::new(
                StatusCode::CONFLICT,
                "Request conflict",
                "The request conflicts with the current resource state.",
                "conflict",
                instance,
            ),
            ServiceError::Invalid(reason) => Self::invalid(instance, "body", reason),
            ServiceError::Store(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
                "The request could not be completed.",
                "internal_error",
                instance,
            ),
        }
    }

    pub fn internal(instance: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error",
            "The request could not be completed.",
            "internal_error",
            instance,
        )
    }

    fn new(
        status: StatusCode,
        title: &'static str,
        detail: impl Into<String>,
        code: &'static str,
        instance: impl Into<String>,
    ) -> Self {
        Self {
            problem_type: format!("{TYPE_ROOT}{code}").into_boxed_str(),
            title,
            status: status.as_u16(),
            detail: detail.into().into_boxed_str(),
            instance: instance.into().into_boxed_str(),
            code,
            invalid_parameters: Vec::new(),
        }
    }
}

impl IntoResponse for ProblemDetails {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let unauthenticated = status == StatusCode::UNAUTHORIZED;
        let mut response = (status, Json(self)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_MEDIA_TYPE),
        );
        if unauthenticated {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"kit\""),
            );
        }
        response
    }
}
