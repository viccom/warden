//! HTTP API:axum router + AppState + token 鉴权中间件。
//!
//! 路由前缀 `/api/v1`。鉴权用静态 token(`daemon.auth_token` 非空时生效),
//! `/api/v1/health` 放白名单。设计见 docs/DESIGN.md §8。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;

use crate::config::Config;
use crate::model::ServiceConfig;
use crate::supervisor::Supervisor;

pub mod auth;
pub mod routes_health;
pub mod routes_logs;
pub mod routes_service;
pub mod routes_ui;

/// 运行时 CRUD 服务 overlay 文件名(存 data_dir 下,不动主配置文件)。
pub const RUNTIME_SERVICES_FILE: &str = "runtime_services.toml";

/// 贯穿所有 handler 的共享状态。
#[derive(Clone)]
pub struct AppState {
    pub supervisor: Arc<Supervisor>,
    pub config_path: Option<PathBuf>,
    pub auth_token: Option<String>,
    pub version: &'static str,
    /// data_dir(空 = 未配置,CRUD/desired 仅内存生效)。
    pub data_dir: PathBuf,
    /// 运行时增改的服务 registry(CRUD 持久化的数据源)。
    pub runtime_services: Arc<Mutex<Vec<ServiceConfig>>>,
}

/// 从配置构建 AppState + Supervisor(不启动 auto_start,由调用方决定)。
/// 顺序:烘入 daemon 全局 env → merge 运行时 overlay(按 name 覆盖)→ 建句柄。
pub fn build_state(cfg: Config, config_path: Option<PathBuf>) -> AppState {
    let mut cfg = cfg;
    cfg.apply_daemon_env();
    let data_dir = if cfg.daemon.data_dir.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(&cfg.daemon.data_dir)
    };
    // 运行时 overlay:CRUD 增改的服务按 name 覆盖主配置(重启后恢复)
    let runtime_services = read_runtime_services(&data_dir);
    merge_runtime(&mut cfg, &runtime_services);
    let supervisor = Arc::new(Supervisor::from_config(&cfg, data_dir.clone()));
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
        data_dir,
        runtime_services: Arc::new(Mutex::new(runtime_services)),
    }
}

/// 读取 data_dir/runtime_services.toml(不存在/解析失败返回空并 warn)。
fn read_runtime_services(data_dir: &std::path::Path) -> Vec<ServiceConfig> {
    if data_dir.as_os_str().is_empty() {
        return Vec::new();
    }
    let p = data_dir.join(RUNTIME_SERVICES_FILE);
    match std::fs::read_to_string(&p) {
        Ok(s) => match toml::from_str::<HashMap<String, Vec<ServiceConfig>>>(&s) {
            Ok(m) => m.into_iter().next().map(|(_, v)| v).unwrap_or_default(),
            Err(e) => {
                tracing::warn!("[api] 运行时服务文件解析失败({}):{e}", p.display());
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// overlay 按覆盖主配置(运行时增改优先)。
fn merge_runtime(cfg: &mut Config, runtime: &[ServiceConfig]) {
    for svc in runtime {
        if let Some(existing) = cfg.services.iter_mut().find(|s| s.name == svc.name) {
            *existing = svc.clone();
        } else {
            cfg.services.push(svc.clone());
        }
    }
}

/// 把 registry 持久化到 data_dir/runtime_services.toml(原子性:直接写,文件小)。
pub fn persist_runtime(state: &AppState) {
    if state.data_dir.as_os_str().is_empty() {
        tracing::warn!("[api] data_dir 未配置,运行时变更仅内存生效(重启丢失)");
        return;
    }
    let list = state.runtime_services.lock().unwrap().clone();
    let toml = toml::to_string(&HashMap::from([("service", list)])).unwrap_or_default();
    let p = state.data_dir.join(RUNTIME_SERVICES_FILE);
    if let Err(e) = std::fs::write(&p, toml) {
        tracing::warn!("[api] 运行时服务落盘失败({}):{e}", p.display());
    }
}

pub fn build_router(state: AppState) -> Router {
    use axum::routing::{delete, get, post, put};
    Router::new()
        .route("/", get(routes_ui::index))
        .route("/api/v1/health", get(routes_health::health))
        .route("/api/v1/services", get(routes_service::list))
        .route("/api/v1/services", post(routes_service::create))
        .route(
            "/api/v1/services/start-all",
            post(routes_service::start_all),
        )
        .route("/api/v1/services/stop-all", post(routes_service::stop_all))
        .route("/api/v1/services/{name}", get(routes_service::get_one))
        .route("/api/v1/services/{name}", put(routes_service::update))
        .route("/api/v1/services/{name}", delete(routes_service::delete))
        .route(
            "/api/v1/services/{name}/config",
            get(routes_service::get_config),
        )
        .route("/api/v1/services/{name}/start", post(routes_service::start))
        .route("/api/v1/services/{name}/stop", post(routes_service::stop))
        .route(
            "/api/v1/services/{name}/restart",
            post(routes_service::restart),
        )
        .route("/api/v1/services/{name}/logs", get(routes_logs::logs))
        .route(
            "/api/v1/services/{name}/logs/stream",
            get(routes_logs::logs_stream),
        )
        .route(
            "/api/v1/services/{name}/metrics",
            get(routes_service::metrics),
        )
        .route("/api/v1/config/reload", post(routes_service::reload))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
