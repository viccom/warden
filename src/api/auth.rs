//! 静态 token 鉴权中间件。

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::AppState;

/// 校验 `Authorization: Bearer <token>`。
/// - 未配置 token(空) → 全部放行
/// - `/api/v1/health` 放行(便于探活)
pub async fn auth_middleware(
    State(st): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // 未配置 token → 不鉴权
    if st.auth_token.is_none() {
        return next.run(req).await;
    }
    // 白名单:UI 页面 + 健康端点(浏览器直接打开 / 与探活,无需 token)
    let path = req.uri().path();
    if path == "/" || path == "/api/v1/health" {
        return next.run(req).await;
    }
    let expected = st.auth_token.as_deref().unwrap_or("");
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.strip_prefix("Bearer ").unwrap_or("") == expected)
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
    }
}
