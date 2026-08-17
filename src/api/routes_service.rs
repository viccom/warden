//! 服务生命周期端点:list / get / start / stop / restart / metrics / start-all / stop-all / reload
//! + 运行时 CRUD(create / update / delete)。
//!
//! 数据源:配置文件是唯一数据源——CRUD 写回 `AppState::effective_config_path()`
//! (toml_edit 保注释,原子写),Supervisor 内存随之同步;start/stop 等运行时
//! 操作不持久化(daemon 重启后按 auto_start 决定拉起,语义与 supervisord 一致)。

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use std::collections::HashSet;

use crate::api::AppState;
use crate::config;
use crate::config_edit::ConfigFile;
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

/// 组级启动:组内按优先级序 + 就绪推进。
pub async fn group_start(
    State(st): State<AppState>,
    Path(group): Path<String>,
) -> WResult<impl IntoResponse> {
    let names = st.supervisor.start_group(&group).await;
    Ok(Json(
        json!({ "status": "group-start done", "group": group, "services": names }),
    ))
}

/// 组级停止:组内逆序(被依赖方最后停)。
pub async fn group_stop(
    State(st): State<AppState>,
    Path(group): Path<String>,
) -> WResult<impl IntoResponse> {
    let names = st.supervisor.stop_group(&group).await;
    Ok(Json(
        json!({ "status": "group-stop done", "group": group, "services": names }),
    ))
}

// ── 运行时 CRUD(写回配置文件,唯一数据源)───────────────────────

/// 新增服务:body = ServiceConfig JSON。校验后写回配置文件 + 注册(不启动)。
/// 文件不可写(只读等)则整体拒绝,内存不动。
pub async fn create(
    State(st): State<AppState>,
    Json(svc): Json<ServiceConfig>,
) -> WResult<impl IntoResponse> {
    let _edit = st.config_edit_lock.lock().unwrap();
    let mut seen = HashSet::new();
    // 与现有服务(含配置文件)查重——锁内做,并发同名 create 后者在写入前被拒
    for name in st.supervisor.names() {
        seen.insert(name);
    }
    config::validate_service(&svc, &mut seen).map_err(WardenError::Config)?;
    let mut file = ConfigFile::load_or_create(&st.effective_config_path())?;
    file.append_service(&svc)?;
    file.save()?;
    st.supervisor.add(svc.clone())?;
    tracing::info!(
        "[config] create '{}' 写回 {}",
        svc.name,
        st.effective_config_path().display()
    );
    Ok(Json(json!({ "status": "created", "name": svc.name })))
}

/// 更新服务配置:任意状态可改(运行中允许保存:进程状态保留,新配置下次
/// start/restart 生效;health/组排序等读 config 的即时生效)。删除仍要求停止。
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
    let _edit = st.config_edit_lock.lock().unwrap();
    let mut file = ConfigFile::load_or_create(&st.effective_config_path())?;
    file.replace_service(&svc)?;
    file.save()?;
    st.supervisor.update(svc.clone())?;
    tracing::info!(
        "[config] update '{}' 写回 {}",
        name,
        st.effective_config_path().display()
    );
    Ok(Json(json!({ "status": "updated", "name": name })))
}

/// 删除服务:仅 Stopped/Failed 可删(运行中返回 InvalidState)。
/// 先移除运行时句柄,再从配置文件删除条目(真删,重启不复活)。
pub async fn delete(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    let _edit = st.config_edit_lock.lock().unwrap();
    st.supervisor.remove(&name)?;
    let mut file = ConfigFile::load_or_create(&st.effective_config_path())?;
    if let Err(e) = file.remove_service(&name) {
        // 句柄已移除但文件未删(如条目缺失):如实报错,重启后该服务会回来
        return Err(WardenError::Config(format!(
            "已从运行时移除,但配置文件删除条目失败({e});重启后服务会恢复"
        )));
    }
    file.save()?;
    tracing::info!(
        "[config] delete '{}' 写回 {}",
        name,
        st.effective_config_path().display()
    );
    Ok(Json(json!({ "status": "deleted", "name": name })))
}

/// 服务配置文件读写上限(防大文件拖垮编辑器/请求体)。
const CONFIG_FILE_MAX_BYTES: usize = 1024 * 1024;

/// 按扩展名识别配置文件格式(toml/json 支持校验与格式化;其余按纯文本编辑)。
fn config_file_format(path: &str) -> &'static str {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".toml") {
        "toml"
    } else if p.ends_with(".json") {
        "json"
    } else if p.ends_with(".yaml") || p.ends_with(".yml") {
        "yaml"
    } else if p.ends_with(".ini") || p.ends_with(".cfg") || p.ends_with(".conf") {
        "ini"
    } else {
        "text"
    }
}

/// 读取服务的配置文件(ServiceConfig.config_file;未配置 → 404)。
/// 文件不存在返回 exists=false + 空内容(编辑器可创建)。
pub async fn get_config_file(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<impl IntoResponse> {
    let h = st.supervisor.handle(&name)?;
    let cfg = h.inner.lock().unwrap().config.clone();
    let path = cfg
        .config_file
        .ok_or_else(|| WardenError::ServiceNotFound(format!("{name} 未配置 config_file")))?;
    // 大文件先按 metadata 快速拒绝,避免整读后才报错
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > CONFIG_FILE_MAX_BYTES as u64 {
            return Err(WardenError::Config(
                "配置文件超过 1MB,不支持在线编辑".into(),
            ));
        }
    }
    let (exists, content) = match std::fs::read_to_string(&path) {
        Ok(c) => (true, c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, String::new()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(WardenError::Config(format!(
                "配置文件不是 UTF-8 文本(GBK 等编码暂不支持在线编辑):{e}"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    if content.len() > CONFIG_FILE_MAX_BYTES {
        return Err(WardenError::Config(
            "配置文件超过 1MB,不支持在线编辑".into(),
        ));
    }
    Ok(Json(json!({
        "path": path,
        "exists": exists,
        "format": config_file_format(&path),
        "content": content,
    })))
}

#[derive(serde::Deserialize)]
pub struct ConfigFileBody {
    pub content: String,
    /// true = 保存前格式化(仅 toml/json;其他格式忽略)。
    #[serde(default)]
    pub format: bool,
}

/// 保存服务的配置文件。toml/json 保存前校验(坏内容 400 拒绝且不落盘);
/// format=true 时以规范格式落盘。父目录不存在则创建。
pub async fn put_config_file(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ConfigFileBody>,
) -> WResult<impl IntoResponse> {
    let h = st.supervisor.handle(&name)?;
    let cfg = h.inner.lock().unwrap().config.clone();
    let path = cfg
        .config_file
        .ok_or_else(|| WardenError::ServiceNotFound(format!("{name} 未配置 config_file")))?;
    if body.content.len() > CONFIG_FILE_MAX_BYTES {
        return Err(WardenError::Config("内容超过 1MB,不支持在线编辑".into()));
    }
    let mut content = body.content;
    match config_file_format(&path) {
        "toml" => {
            let v: toml::Value = toml::from_str(&content)
                .map_err(|e| WardenError::Config(format!("toml 解析失败:{e}")))?;
            if body.format {
                content = toml::to_string(&v).map_err(|e| WardenError::Config(format!("{e}")))?;
            }
        }
        "json" => {
            let v: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| WardenError::Config(format!("json 解析失败:{e}")))?;
            if body.format {
                content = serde_json::to_string_pretty(&v)
                    .map_err(|e| WardenError::Config(format!("{e}")))?;
            }
        }
        // yaml/ini/text:warden 无对应解析器,不校验不格式化,原样保存
        _ => {}
    }
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &content)?;
    Ok(Json(json!({ "path": path, "bytes": content.len() })))
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

/// 重新加载配置文件并增量同步(文件是唯一数据源):
/// 新服务注册 / 消失的服务优雅停止后移除 / 同名服务配置替换(下次启动生效)。
pub async fn reload(State(st): State<AppState>) -> WResult<impl IntoResponse> {
    let cfg = config::Config::load(st.config_path.as_deref())?;
    let count = cfg.services.len();
    st.supervisor.apply_config(&cfg).await;
    Ok(Json(json!({ "status": "reloaded", "services": count })))
}
