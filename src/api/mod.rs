//! HTTP API:axum router + AppState + token 鉴权中间件。
//!
//! 路由前缀 `/api/v1`。鉴权用静态 token(`daemon.auth_token` 非空时生效),
//! `/api/v1/health` 放白名单。设计见 docs/DESIGN.md §8。

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;

use crate::config::Config;
use crate::supervisor::Supervisor;

pub mod auth;
pub mod routes_health;
pub mod routes_logs;
pub mod routes_service;

/// 贯穿所有 handler 的共享状态。
#[derive(Clone)]
pub struct AppState {
    pub supervisor: Arc<Supervisor>,
    pub config_path: Option<PathBuf>,
    pub auth_token: Option<String>,
    pub version: &'static str,
}

/// 从配置构建 AppState + Supervisor(不启动 auto_start,由调用方决定)。
pub fn build_state(cfg: Config, config_path: Option<PathBuf>) -> AppState {
    let data_dir = if cfg.daemon.data_dir.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(&cfg.daemon.data_dir)
    };
    let supervisor = Arc::new(Supervisor::from_config(&cfg, data_dir));
    let auth_token = if cfg.daemon.auth_token.is_empty() {
        None
    } else {
        Some(cfg.daemon.auth_token.clone())
    };
    AppState {
        supervisor,
        config_path,
        auth_token,
        version: crate::VERSION,
    }
}

pub fn build_router(state: AppState) -> Router {
    use axum::routing::{get, post};
    Router::new()
        .route("/api/v1/health", get(routes_health::health))
        .route("/api/v1/services", get(routes_service::list))
        .route("/api/v1/services/start-all", post(routes_service::start_all))
        .route("/api/v1/services/stop-all", post(routes_service::stop_all))
        .route("/api/v1/services/{name}", get(routes_service::get_one))
        .route("/api/v1/services/{name}/start", post(routes_service::start))
        .route("/api/v1/services/{name}/stop", post(routes_service::stop))
        .route("/api/v1/services/{name}/restart", post(routes_service::restart))
        .route("/api/v1/services/{name}/logs", get(routes_logs::logs))
        .route(
            "/api/v1/services/{name}/logs/stream",
            get(routes_logs::logs_stream),
        )
        .route("/api/v1/services/{name}/metrics", get(routes_service::metrics))
        .route("/api/v1/config/reload", post(routes_service::reload))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
