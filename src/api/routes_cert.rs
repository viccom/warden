//! 证书编排器端点(P6,feature 门控):GET /api/v1/proxy/cert(状态聚合)
//! + POST issue / renew / lego install(单任务互斥,运行中 409)。
//!
//! 数据源照 routes_proxy 模式:触发时重读文件配置(唯一数据源);
//! 凭据经 body 进、落 0600 的 acme.env,GET 永不回显(C3)。
//! 长任务异步执行(编排逻辑见 proxy::certmgr),面板轮询 GET 观测。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;
use crate::config;
use crate::error::{WResult, WardenError};
use crate::lock::lock;
use crate::proxy::certmgr::{self, IssueParams, OrchestratorCtx, ProcessRunner, TaskKind};

/// GET:证书 / lego / acme(非敏感)/ 最近任务 状态聚合。
pub async fn get_cert(State(st): State<AppState>) -> WResult<impl IntoResponse> {
    let pc = super::routes_proxy::file_proxy(&st)?;
    // lego 检测(--version 快速验证;30s 缓存防轮询反复 spawn)
    let probe = Arc::new(ProcessRunner {
        timeout: Duration::from_secs(15),
    });
    let lego = st
        .cert
        .lego_status(probe.as_ref(), &pc.acme, &st.data_dir)
        .await;
    // 证书状态(配置路径或默认路径)
    let (cert_path, _) = cert_target_paths(&pc, &st.data_dir);
    let cert_path_str = cert_path.display().to_string();
    let cert = match certmgr::cert_summary(&cert_path) {
        Ok(s) => json!({
            "exists": true, "path": cert_path_str,
            "days_left": s.days_left, "not_after": s.not_after, "san": s.san,
        }),
        Err(_) => json!({"exists": false, "path": cert_path_str}),
    };
    let task = st.cert.tasks().snapshot(60).map(|t| {
        json!({
            "kind": t.kind, "status": t.status,
            "started_at": t.started_at.to_rfc3339(),
            "finished_at": t.finished_at.map(|f| f.to_rfc3339()),
            "exit": t.exit, "message": t.message,
            "output_tail": t.output_tail, "total_lines": t.total_lines,
        })
    });
    let a = &pc.acme;
    // 凭据文件状态(仅 存在性+条目数,键名/值永不回显,C3)
    let env_file = certmgr::resolve_env_file(a, &st.effective_config_path());
    let env_count = std::fs::read_to_string(&env_file)
        .map(|t| {
            t.lines()
                .filter(|l| {
                    let l = l.trim();
                    !l.is_empty() && !l.starts_with('#') && l.contains('=')
                })
                .count()
        })
        .unwrap_or(0);
    Ok(Json(json!({
        "cert": cert,
        "lego": {
            "installed": lego.installed, "path": lego.path,
            "version": lego.version, "source": lego.source.map(|s| s.as_str()),
        },
        "acme": {
            "email": a.email, "server": a.server, "dns_provider": a.dns_provider,
            "env_file": a.env_file, "lego_path": a.lego_path,
            "renew_days": a.renew_days, "expire_warn_days": a.expire_warn_days,
            "renew_command": a.renew_command,
            "lego_version": a.lego_version, "lego_mirror": a.lego_mirror,
            // 编排配置完整 = 自动续期由 warden 驱动(C5)
            "configured": a.email.is_some() && a.dns_provider.is_some(),
        },
        "env": {
            "file": env_file.display().to_string(),
            "exists": env_file.is_file(),
            "count": env_count,
        },
        "domain": pc.domain,
        "https_bind": pc.https_bind,
        "https_running": st.cert.https_running(),
        "task": task,
    })))
}

/// POST issue body。`env` 为 DNS provider 凭据键值(落 acme.env)。
#[derive(Deserialize)]
pub struct IssueBody {
    pub email: String,
    pub dns_provider: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default = "default_persist")]
    pub persist: bool,
}

fn default_persist() -> bool {
    true
}

/// 一键签发:校验 → 占任务(409)→ 异步执行编排。
pub async fn post_issue(
    State(st): State<AppState>,
    Json(b): Json<IssueBody>,
) -> WResult<impl IntoResponse> {
    // 参数校验(400)
    let email = b.email.trim().to_string();
    if !email.contains('@') || email.len() < 5 {
        return Err(WardenError::Config("email 不合法(须含 @)".into()));
    }
    let dns = b.dns_provider.trim().to_lowercase();
    if dns.is_empty()
        || !dns
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(WardenError::Config(format!(
            "dns_provider '{dns}' 不合法(lego provider 名,如 tencentcloud)"
        )));
    }
    let mut env = BTreeMap::new();
    for (k, v) in &b.env {
        certmgr::validate_env_entry(k, v).map_err(WardenError::Config)?;
        env.insert(k.clone(), v.clone());
    }
    let pc = super::routes_proxy::file_proxy(&st)?;
    // 域名推导前置校验(缺省推导依赖 [proxy] domain)
    certmgr::derive_domains(b.domains.clone(), pc.domain.as_deref())
        .map_err(WardenError::Config)?;

    // 单任务互斥(409)→ 异步执行
    st.cert
        .tasks()
        .try_begin(TaskKind::Issue)
        .map_err(WardenError::Conflict)?;
    let ctx = orchestrator_ctx(&st, pc);
    tokio::spawn(certmgr::execute_issue(
        st.cert.clone(),
        task_runner(),
        ctx,
        IssueParams {
            email,
            dns_provider: dns,
            server: b.server,
            domains: b.domains,
            env,
            persist: b.persist,
        },
    ));
    tracing::info!("[cert] 一键签发任务已启动");
    Ok(Json(json!({"status": "started", "kind": "issue"})))
}

/// 手动续期:配置须完整(email + dns_provider);单任务 409 同上。
pub async fn post_renew(State(st): State<AppState>) -> WResult<impl IntoResponse> {
    let pc = super::routes_proxy::file_proxy(&st)?;
    if pc.acme.email.is_none() || pc.acme.dns_provider.is_none() {
        return Err(WardenError::Config(
            "未配置 [proxy.acme] email/dns_provider,无法续期(先一键签发或补全配置)".into(),
        ));
    }
    st.cert
        .tasks()
        .try_begin(TaskKind::Renew)
        .map_err(WardenError::Conflict)?;
    let ctx = orchestrator_ctx(&st, pc);
    tokio::spawn(certmgr::execute_renew(st.cert.clone(), task_runner(), ctx));
    tracing::info!("[cert] 手动续期任务已启动");
    Ok(Json(json!({"status": "started", "kind": "renew"})))
}

/// 自动安装 lego(GitHub latest → data/bin;内网不可达时任务面板回退指引)。
pub async fn post_lego_install(State(st): State<AppState>) -> WResult<impl IntoResponse> {
    st.cert
        .tasks()
        .try_begin(TaskKind::Install)
        .map_err(WardenError::Conflict)?;
    // 版本规格/镜像随文件配置(触发时现读,运行期改配置即时生效)
    let acme = super::routes_proxy::file_proxy(&st)?.acme;
    tokio::spawn(certmgr::execute_install(
        st.cert.clone(),
        task_runner(),
        acme,
        st.data_dir.clone(),
    ));
    tracing::info!("[cert] lego 自动安装任务已启动");
    Ok(Json(json!({"status": "started", "kind": "install"})))
}

/// PUT /cert/acme body(设置面全量语义):空字符串字段 = 清空该配置;
/// renew_days 缺省(None)= 不动;domain 空串 = 清空根域。
#[derive(Deserialize)]
pub struct AcmeSettingsBody {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub dns_provider: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub renew_days: Option<u32>,
    #[serde(default)]
    pub domain: String,
}

/// 编排设置写回:校验 → 保注释写回([proxy] domain + [proxy.acme] 非敏感字段)
/// → 引擎热生效(domain 参与 auto 路由)。凭据不经此端点(只在 issue 时落 acme.env)。
pub async fn put_acme(
    State(st): State<AppState>,
    Json(b): Json<AcmeSettingsBody>,
) -> WResult<impl IntoResponse> {
    let email = b.email.trim().to_string();
    if !email.is_empty() && (!email.contains('@') || email.len() < 5) {
        return Err(WardenError::Config("email 不合法(须含 @)".into()));
    }
    let dns = b.dns_provider.trim().to_lowercase();
    if !dns.is_empty()
        && !dns
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(WardenError::Config(format!(
            "dns_provider '{dns}' 不合法(lego provider 名,如 tencentcloud)"
        )));
    }
    if let Some(days) = b.renew_days {
        if days > 90 {
            return Err(WardenError::Config(format!(
                "renew_days {days} 不合法(证书有效期 90 天,1..=90;0 = 关闭)"
            )));
        }
    }
    // 根域归一:剥 `*.` 前缀(设置面写「泛域名根域」的自然输入)+ 小写化
    let domain = b.domain.trim().trim_start_matches("*.").to_lowercase();
    if !domain.is_empty() && domain.contains(['/', '\\', ':', '*']) {
        return Err(WardenError::Config(format!(
            "domain '{domain}' 非法(须为纯域名,无 path/端口/通配符)"
        )));
    }
    let server = if b.server.trim().is_empty() {
        "letsencrypt".to_string()
    } else {
        b.server.trim().to_string()
    };

    let _edit = lock(&st.config_edit_lock);
    let mut file = crate::config_edit::ConfigFile::load_or_create(&st.effective_config_path())?;
    file.set_proxy_domain(&domain)?;
    file.upsert_proxy_acme(&crate::config_edit::AcmePersist {
        email: Some(email),
        dns_provider: Some(dns),
        server: Some(server),
        env_file: None, // env_file 路径缺省跟随配置目录,不在此改
        renew_days: b.renew_days,
    })?;
    file.save()?;
    let engine = super::routes_proxy::sync_engine(&st)?;
    tracing::info!(
        "[config] cert-acme 设置写回 {}(engine={engine})",
        st.effective_config_path().display()
    );
    Ok(Json(json!({ "status": "updated", "engine": engine })))
}

/// 编排任务用的真实执行器(600s 超时,对齐 LEGO_TIMEOUT)。
fn task_runner() -> Arc<ProcessRunner> {
    Arc::new(ProcessRunner::default())
}

/// 触发时组装编排上下文(现读文件配置,运行期改配置即时生效)。
fn orchestrator_ctx(st: &AppState, pc: config::ProxyConfig) -> OrchestratorCtx {
    OrchestratorCtx {
        data_dir: st.data_dir.clone(),
        config_path: st.effective_config_path(),
        cert_file: pc.cert_file.clone(),
        key_file: pc.key_file.clone(),
        domain: pc.domain.clone(),
        acme: pc.acme.clone(),
        edit_lock: st.config_edit_lock.clone(),
    }
}

/// 证书目标路径(配置的 cert_file/key_file 或默认 `<data_dir>/ssl/`)。
fn cert_target_paths(
    pc: &config::ProxyConfig,
    data_dir: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    match (&pc.cert_file, &pc.key_file) {
        (Some(c), Some(k)) => (c.into(), k.into()),
        _ => certmgr::default_cert_paths(data_dir),
    }
}
