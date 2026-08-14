//! 服务生命周期端点:list / get / start / stop / restart / metrics / start-all / stop-all / reload。

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::api::AppState;
use crate::config::Config;
use crate::error::WResult;

pub async fn list(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "services": st.supervisor.list() }))
}

pub async fn get_one(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    let s = st.supervisor.status(&name)?;
    Ok(Json(json!(s)))
}

pub async fn start(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.start(&name).await?;
    Ok(Json(json!({ "status": "started", "name": name })))
}

pub async fn stop(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.stop(&name).await?;
    Ok(Json(json!({ "status": "stopped", "name": name })))
}

pub async fn restart(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.restart(&name).await?;
    Ok(Json(json!({ "status": "restarted", "name": name })))
}

pub async fn start_all(State(st): State<AppState>) -> impl IntoResponse {
    st.supervisor.start_all().await;
    Json(json!({ "status": "start-all done" }))
}

pub async fn stop_all(State(st): State<AppState>) -> impl IntoResponse {
    st.supervisor.stop_all().await;
    Json(json!({ "status": "stop-all done" }))
}

pub async fn metrics(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    let s = st.supervisor.status(&name)?;
    Ok(Json(json!({ "name": name, "state": s.state, "metrics": s.metrics })))
}

/// 重新加载配置文件,增量同步(add 新服务 / remove 已停止的旧服务,运行中保留)。
pub async fn reload(State(st): State<AppState>) -> WResult<impl IntoResponse> {
    let cfg = Config::load(st.config_path.as_deref())?;
    let count = cfg.services.len();
    st.supervisor.apply_config(&cfg);
    Ok(Json(json!({ "status": "reloaded", "services": count })))
}
