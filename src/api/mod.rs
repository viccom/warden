//! HTTP API:axum router + AppState + token 鉴权中间件。
//!
//! 路由前缀 `/api/v1`。鉴权用静态 token(`daemon.auth_token` 非空时生效),
//! `/api/v1/health` 放白名单。设计见 docs/DESIGN.md §8。
//!
//! 数据源约定:配置文件是服务定义的**唯一数据源**——CRUD 直接写回文件
//! (`config_edit`,toml_edit 保注释),内存(Supervisor)随之同步;
//! 无 overlay、无 desired_state(旧文件由 `config_edit::migrate_legacy_sources`
//! 一次性迁移)。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;

use crate::config::Config;
use crate::supervisor::Supervisor;

pub mod auth;
pub mod routes_health;
pub mod routes_logs;
pub mod routes_service;
pub mod routes_ui;

/// 贯穿所有 handler 的共享状态。
#[derive(Clone)]
pub struct AppState {
    pub supervisor: Arc<Supervisor>,
    pub config_path: Option<PathBuf>,
    pub auth_token: Option<String>,
    pub version: &'static str,
    /// data_dir(空 = 未配置,仅内存生效)。
    pub data_dir: PathBuf,
    /// 配置文件写互斥锁(CRUD 进程内单写者,防并发编辑交错)。
    pub config_edit_lock: Arc<Mutex<()>>,
}

impl AppState {
    /// CRUD 写回目标配置文件:显式/已定位的配置文件;全无则默认创建路径
    /// (`config/services.toml`,与查找链第 3 级一致)。
    pub fn effective_config_path(&self) -> PathBuf {
        self.config_path
            .clone()
            .unwrap_or_else(crate::config::default_config_create_path)
    }
}

/// 从配置构建 AppState + Supervisor(不启动 auto_start,由调用方决定)。
/// 顺序:烘入 daemon 全局 env → 解析配置路径(显式 → find 查找)→
/// 旧双源文件一次性迁移 → 建句柄。
pub fn build_state(cfg: Config, config_path: Option<PathBuf>) -> AppState {
    let mut cfg = cfg;
    cfg.apply_daemon_env();
    let data_dir = if cfg.daemon.data_dir.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(&cfg.daemon.data_dir)
    };
    // CLI/Service 未显式传路径时,补上 find 链实际命中的文件(CRUD/reload 的写读目标)
    let config_path = config_path.or_else(Config::find_config_path);
    // 旧 runtime overlay / desired_state 一次性迁入配置文件(存在才动)
    let config_path = crate::config_edit::migrate_legacy_sources(&mut cfg, config_path, &data_dir);
    // 迁移可能替换了 services(未烘 env),再烘一次(幂等)
    cfg.apply_daemon_env();
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
        config_edit_lock: Arc::new(Mutex::new(())),
    }
}

/// 桌面版(Tauri webview)跨域访问白名单:仅放行 Tauri 相关 origin,不开放任意来源
/// (防浏览器端任意页面打无 token 的本地 daemon)。
/// - `http://tauri.localhost`:Windows WebView2 的页面 origin
/// - `tauri://localhost`:macOS/Linux webview
/// - `http://localhost:1420`:桌面版 `tauri dev`(vite dev server)
fn tauri_cors() -> tower_http::cors::CorsLayer {
    use axum::http::{header, Method};
    use tower_http::cors::CorsLayer;
    let origins = [
        "http://tauri.localhost".parse().unwrap(),
        "tauri://localhost".parse().unwrap(),
        "http://localhost:1420".parse().unwrap(),
    ];
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
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
        .route(
            "/api/v1/groups/{group}/start",
            post(routes_service::group_start),
        )
        .route(
            "/api/v1/groups/{group}/stop",
            post(routes_service::group_stop),
        )
        .route("/api/v1/services/{name}", get(routes_service::get_one))
        .route("/api/v1/services/{name}", put(routes_service::update))
        .route("/api/v1/services/{name}", delete(routes_service::delete))
        .route(
            "/api/v1/services/{name}/config",
            get(routes_service::get_config),
        )
        .route(
            "/api/v1/services/{name}/config-file",
            get(routes_service::get_config_file).put(routes_service::put_config_file),
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
        // CORS 在 auth 外层:预检(OPTIONS)由 CORS 直接应答,不进鉴权
        .layer(tauri_cors())
        // 请求级日志提到 INFO(TraceLayer 默认 DEBUG 会被 EnvFilter=info 滤掉):
        // 每个请求一行(span 带 method/path,response 事件带 status/耗时),现场排障关键线索
        .layer(tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(|req: &axum::http::Request<axum::body::Body>| {
                tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
            })
            .on_response(log_response))
        .with_state(state)
}

/// 每响应一行 INFO(method/path 在 span 上下文里):具名泛型函数满足
/// tower-http OnResponse 的高阶生命周期约束(闭包写法会撞 FnOnce 不够泛)。
fn log_response<B>(
    resp: &axum::http::Response<B>,
    latency: std::time::Duration,
    _span: &tracing::Span,
) {
    tracing::info!(
        status = resp.status().as_u16(),
        latency_ms = latency.as_millis() as u64,
        "response"
    );
}
