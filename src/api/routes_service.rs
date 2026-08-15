//! 服务生命周期端点:list / get / start / stop / restart / metrics / start-all / stop-all / reload
//! + 运行时 CRUD(create / update / delete)。
//!
//! desired-state 标记:仅用户显式操作(start/stop/restart/start-all/stop-all)写入
//! desired_state.json;daemon 优雅停机的 stop_all(在 lib.rs)不清除,重启后由
//! start_desired 恢复。

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use std::collections::HashSet;

use crate::api::{persist_runtime, AppState};
use crate::config;
use crate::error::{WResult, WardenError};
use crate::model::ServiceConfig;

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

/// 服务完整配置(编辑表单预填;list 只返回状态快照,不含 command/env 等)。
pub async fn get_config(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    let h = st.supervisor.handle(&name)?;
    let cfg = h.inner.lock().unwrap().config.clone();
    Ok(Json(cfg))
}

pub async fn start(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.start(&name).await?;
    st.supervisor.set_desired(&name, true);
    Ok(Json(json!({ "status": "started", "name": name })))
}

pub async fn stop(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.stop(&name).await?;
    st.supervisor.set_desired(&name, false);
    Ok(Json(json!({ "status": "stopped", "name": name })))
}

pub async fn restart(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.restart(&name).await?;
    st.supervisor.set_desired(&name, true);
    Ok(Json(json!({ "status": "restarted", "name": name })))
}

pub async fn start_all(State(st): State<AppState>) -> impl IntoResponse {
    st.supervisor.start_all().await;
    for name in st.supervisor.names() {
        st.supervisor.set_desired(&name, true);
    }
    Json(json!({ "status": "start-all done" }))
}

pub async fn stop_all(State(st): State<AppState>) -> impl IntoResponse {
    st.supervisor.stop_all().await;
    for name in st.supervisor.names() {
        st.supervisor.set_desired(&name, false);
    }
    Json(json!({ "status": "stop-all done" }))
}

/// 组级启动:组内按优先级序 + 就绪推进;desired 随组操作同步。
pub async fn group_start(
    State(st): State<AppState>,
    Path(group): Path<String>,
) -> WResult<impl IntoResponse> {
    let names = st.supervisor.start_group(&group).await;
    for n in &names {
        st.supervisor.set_desired(n, true);
    }
    Ok(Json(
        json!({ "status": "group-start done", "group": group, "services": names }),
    ))
}

/// 组级停止:组内逆序(被依赖方最后停);desired 随组操作同步。
pub async fn group_stop(
    State(st): State<AppState>,
    Path(group): Path<String>,
) -> WResult<impl IntoResponse> {
    let names = st.supervisor.stop_group(&group).await;
    for n in &names {
        st.supervisor.set_desired(n, false);
    }
    Ok(Json(
        json!({ "status": "group-stop done", "group": group, "services": names }),
    ))
}

// ── 运行时 CRUD ────────────────────────────────────────────────

/// 新增服务:body = ServiceConfig JSON。校验后注册(不启动)+ 落盘 overlay。
pub async fn create(
    State(st): State<AppState>,
    Json(svc): Json<ServiceConfig>,
) -> WResult<impl IntoResponse> {
    let mut seen = HashSet::new();
    // 与现有服务(含主配置)查重
    for name in st.supervisor.names() {
        seen.insert(name);
    }
    config::validate_service(&svc, &mut seen).map_err(WardenError::Config)?;
    st.supervisor.add(svc.clone());
    {
        let mut reg = st.runtime_services.lock().unwrap();
        reg.retain(|s| s.name != svc.name);
        reg.push(svc.clone());
    }
    persist_runtime(&st);
    Ok(Json(json!({ "status": "created", "name": svc.name })))
}

/// 更新服务配置:仅 Stopped/Failed 可改(运行中返回 InvalidState)。
/// body = 新 ServiceConfig,name 必须与路径一致。
pub async fn update(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Json(svc): Json<ServiceConfig>,
) -> WResult<impl IntoResponse> {
    if svc.name != name {
        return Err(WardenError::Config(format!(
            "body.name '{}' 与路径 '{name}' 不一致",
            svc.name
        )));
    }
    let mut seen = HashSet::new();
    config::validate_service(&svc, &mut seen).map_err(WardenError::Config)?;
    st.supervisor.update(svc.clone())?;
    {
        let mut reg = st.runtime_services.lock().unwrap();
        reg.retain(|s| s.name != name);
        reg.push(svc.clone());
    }
    persist_runtime(&st);
    Ok(Json(json!({ "status": "updated", "name": name })))
}

/// 删除服务:仅 Stopped/Failed 可删。同时从 overlay 移除(主配置文件中的服务
/// 重启 daemon 后会回来——删除主配置服务需直接改文件)。
pub async fn delete(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    st.supervisor.remove(&name)?;
    {
        let mut reg = st.runtime_services.lock().unwrap();
        reg.retain(|s| s.name != name);
    }
    persist_runtime(&st);
    st.supervisor.set_desired(&name, false);
    Ok(Json(json!({ "status": "deleted", "name": name })))
}

pub async fn metrics(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    let s = st.supervisor.status(&name)?;
    Ok(Json(
        json!({ "name": name, "state": s.state, "metrics": s.metrics }),
    ))
}

/// 重新加载配置文件,增量同步(add 新服务 / remove 已停止的旧服务,运行中保留)。
/// 注意:运行时 overlay 的服务不在主配置文件中,增量同步不会移除活跃服务,故保留。
pub async fn reload(State(st): State<AppState>) -> WResult<impl IntoResponse> {
    let cfg = config::Config::load(st.config_path.as_deref())?;
    let count = cfg.services.len();
    st.supervisor.apply_config(&cfg);
    Ok(Json(json!({ "status": "reloaded", "services": count })))
}
