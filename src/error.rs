//! warden 错误类型。
//!
//! 库层用 `thiserror` 枚举 `WardenError`;binary 层用 `anyhow` 聚合。
//! HTTP 边界实现 `IntoResponse`,统一把错误转成 JSON 响应(改进点:rs-iot 没做)。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// warden 库错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WardenError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("service not found: {0}")]
    ServiceNotFound(String),

    #[error("service '{0}' {1}")]
    InvalidState(String, String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

impl WardenError {
    /// 错误码(slug)与 HTTP 状态码。
    fn parts(&self) -> (&'static str, StatusCode) {
        use WardenError::*;
        match self {
            ServiceNotFound(_) => ("not_found", StatusCode::NOT_FOUND),
            InvalidState(..) => ("invalid_state", StatusCode::CONFLICT),
            Unauthorized => ("unauthorized", StatusCode::UNAUTHORIZED),
            Config(_) => ("config", StatusCode::BAD_REQUEST),
            Io(_) | Toml(_) | Json(_) | Internal(_) => {
                ("internal", StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

impl IntoResponse for WardenError {
    fn into_response(self) -> Response {
        let (error, code) = self.parts();
        let body = ErrorBody {
            error,
            message: self.to_string(),
        };
        (code, Json(body)).into_response()
    }
}

pub type WResult<T> = std::result::Result<T, WardenError>;
