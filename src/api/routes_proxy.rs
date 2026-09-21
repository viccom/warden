//! 反代管理端点(P5,feature 门控):GET /api/v1/proxy(全局 + 路由 + metrics)
//! + 路由 CRUD。
//!
//! 数据源与既有服务 CRUD 同构:配置文件是唯一数据源(toml_edit 保注释写回),
//! 写回后同步 SharedProxyConfig 引擎即时生效;[proxy] 段未配置(引擎未运行)
//! 时仍可编辑文件,返回 restart_required 提示。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::api::AppState;
use crate::config::{self, ProxyRoute};
use crate::config_edit::ConfigFile;
use crate::error::{WResult, WardenError};
use crate::lock::{lock, read, write};

/// 全局视图:engine 状态 + domain/binds/preserve_host + 路由表 + 路由级 metrics。
pub async fn get_proxy(State(st): State<AppState>) -> impl IntoResponse {
    let cfg = st.proxy_shared.as_ref().map(|s| read(s).clone());
    let metrics: Vec<_> = st
        .proxy_metrics
        .snapshot()
        .into_iter()
        .map(|(route, requests, server_errors)| {
            json!({ "route": route, "requests": requests, "server_errors": server_errors })
        })
        .collect();
    let routes = cfg.as_ref().map_or(Vec::new(), |c| c.routes.clone());
    Json(json!({
        "enabled": st.proxy_shared.is_some(),
        "domain": cfg.as_ref().and_then(|c| c.domain.clone()),
        "http_bind": cfg.as_ref().and_then(|c| c.http_bind.clone()),
        "https_bind": cfg.as_ref().and_then(|c| c.https_bind.clone()),
        "preserve_host": cfg.as_ref().is_some_and(|c| c.preserve_host),
        "routes": routes,
        "metrics": metrics,
    }))
}

/// 新增路由:校验(含 host 查重)→ 写回文件 → 引擎同步。
pub async fn create_route(
    State(st): State<AppState>,
    Json(route): Json<ProxyRoute>,
) -> WResult<impl IntoResponse> {
    let _edit = lock(&st.config_edit_lock);
    let mut route = route;
    route.host = route.host.to_lowercase();
    validate_new(&st, &route)?;
    let mut file = ConfigFile::load_or_create(&st.effective_config_path())?;
    file.upsert_proxy_route(&route)?;
    file.save()?;
    let engine = sync_engine(&st)?;
    tracing::info!(
        "[config] proxy-route create '{}' 写回 {}",
        route.host,
        st.effective_config_path().display()
    );
    Ok(Json(
        json!({ "status": "created", "host": route.host, "engine": engine }),
    ))
}

/// 更新路由(host 为键,不可改名——改名 = 删旧建新)。body.host 须与路径一致。
pub async fn update_route(
    State(st): State<AppState>,
    Path(host): Path<String>,
    Json(route): Json<ProxyRoute>,
) -> WResult<impl IntoResponse> {
    if route.host.to_lowercase() != host.to_lowercase() {
        return Err(WardenError::Config(format!(
            "body.host '{}' 与路径 '{host}' 不一致(改 host 请删旧建新)",
            route.host
        )));
    }
    let _edit = lock(&st.config_edit_lock);
    let mut route = route;
    route.host = host.to_lowercase();
    let cur = file_proxy(&st)?;
    if !cur.routes.iter().any(|r| r.host == route.host) {
        return Err(WardenError::NotFound(format!(
            "路由 '{}' 不存在",
            route.host
        )));
    }
    let warns = validate_route(&st, &route, &cur);
    if let Some(e) = warns.first() {
        return Err(WardenError::Config(e.clone()));
    }
    let mut file = ConfigFile::load_or_create(&st.effective_config_path())?;
    file.upsert_proxy_route(&route)?;
    file.save()?;
    let engine = sync_engine(&st)?;
    tracing::info!("[config] proxy-route update '{}' 写回", route.host);
    Ok(Json(
        json!({ "status": "updated", "host": route.host, "engine": engine }),
    ))
}

/// 删除路由(不存在 404 语义:Config 错误透传)。
pub async fn delete_route(
    State(st): State<AppState>,
    Path(host): Path<String>,
) -> WResult<impl IntoResponse> {
    let _edit = lock(&st.config_edit_lock);
    let mut file = ConfigFile::load_or_create(&st.effective_config_path())?;
    let host = host.to_lowercase();
    if !file.remove_proxy_route(&host)? {
        return Err(WardenError::NotFound(format!("路由 '{host}' 不存在")));
    }
    file.save()?;
    let engine = sync_engine(&st)?;
    tracing::info!("[config] proxy-route delete '{host}' 写回");
    Ok(Json(
        json!({ "status": "deleted", "host": host, "engine": engine }),
    ))
}

/// 校验新路由:格式(单条规则)+ 对现有表查重。
fn validate_new(st: &AppState, route: &ProxyRoute) -> WResult<()> {
    let cur = file_proxy(st)?;
    if cur.routes.iter().any(|r| r.host == route.host) {
        return Err(WardenError::Conflict(format!(
            "路由 host '{}' 已存在(重复 host 命中非确定)",
            route.host
        )));
    }
    let warns = validate_route(st, route, &cur);
    if let Some(e) = warns.first() {
        return Err(WardenError::Config(e.clone()));
    }
    Ok(())
}

/// 单条路由校验:复用配置校验规则 + 服务引用存在性(运行时服务表)。
fn validate_route(st: &AppState, route: &ProxyRoute, cur: &config::ProxyConfig) -> Vec<String> {
    let names = st.supervisor.names();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    config::validate_proxy_route(route, cur.domain.as_deref(), &refs)
}

/// 当前文件中的 [proxy](文件是唯一数据源;与 shared 的差异以文件为准重建)。
fn file_proxy(st: &AppState) -> WResult<config::ProxyConfig> {
    let cfg = config::Config::load(st.config_path.as_deref())
        .map_err(|e| WardenError::Config(format!("重读配置失败:{e}")))?;
    Ok(cfg.proxy.unwrap_or_else(|| config::ProxyConfig {
        domain: None,
        http_bind: None,
        https_bind: None,
        connect_timeout_ms: 5000,
        preserve_host: false,
        cert_file: None,
        key_file: None,
        upstream_ca_file: None,
        acme: Default::default(),
        routes: Vec::new(),
    }))
}

/// 写回后同步引擎:重读文件 → 写 shared(引擎下一请求即用新路由表)。
/// 返回引擎状态文案(无 [proxy] 启动/段被移除 → restart_required/stale)。
fn sync_engine(st: &AppState) -> WResult<&'static str> {
    let cfg = config::Config::load(st.config_path.as_deref())
        .map_err(|e| WardenError::Config(format!("重读配置失败:{e}")))?;
    match (&st.proxy_shared, cfg.proxy) {
        (Some(shared), Some(p)) => {
            *write(shared) = Arc::new(p);
            Ok("live")
        }
        (None, _) => Ok("restart_required"),
        (Some(_), None) => {
            tracing::warn!("[proxy] 配置文件已无 [proxy] 段,引擎沿用旧配置(彻底移除需重启)");
            Ok("stale(until restart)")
        }
    }
}
