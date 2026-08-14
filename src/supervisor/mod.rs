//! 监护引擎:管理一组被监护服务的生命周期。
//!
//! 每个服务对应一个 `ProcHandle`;`Supervisor::start` 后 spawn 一个
//! `proc::supervise` task,在该 task 内独占子进程、接管 stdout/stderr、
//! 监听退出并按 `RestartPolicy` 决策重启。`stop` 通过 `CancellationToken`
//! 通知 task 主动终止(强制 kill)。设计见 docs/DESIGN.md §6。

pub mod health;
pub mod metrics;
pub mod proc;
pub mod signal;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{WResult, WardenError};
use crate::logs::{LogHub, RollingFile};
use crate::model::{HealthStatus, LastExit, ProcMetrics, ProcState, ServiceConfig};

/// 监护引擎,持有所有被监护服务。
pub struct Supervisor {
    handles: DashMap<String, Arc<ProcHandle>>,
    data_dir: PathBuf,
}

impl Supervisor {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            handles: DashMap::new(),
            data_dir,
        }
    }

    /// 从配置构建:为每个 service 建句柄(不启动)。
    pub fn from_config(cfg: &Config, data_dir: PathBuf) -> Self {
        let sv = Self::new(data_dir);
        for svc in &cfg.services {
            sv.add(svc.clone());
        }
        sv
    }

    /// 注册一个服务(不启动)。
    pub fn add(&self, config: ServiceConfig) {
        let name = config.name.clone();
        let log = Arc::new(LogHub::new(self.make_log_file(&name)));
        let handle = Arc::new(ProcHandle::new(config, log));
        self.handles.insert(name, handle);
    }

    fn make_log_file(&self, name: &str) -> Option<RollingFile> {
        if self.data_dir.as_os_str().is_empty() {
            None
        } else {
            Some(RollingFile::new(
                self.data_dir.join("logs"),
                name.to_string(),
            ))
        }
    }

    fn get(&self, name: &str) -> WResult<Arc<ProcHandle>> {
        self.handles
            .get(name)
            .map(|r| Arc::clone(&r))
            .ok_or_else(|| WardenError::ServiceNotFound(name.into()))
    }

    /// 公开访问句柄(API 读取完整配置等)。
    pub fn handle(&self, name: &str) -> WResult<Arc<ProcHandle>> {
        self.get(name)
    }

    /// 启动指定服务(若已运行/启动中/重启中则返回 InvalidState)。
    pub async fn start(&self, name: &str) -> WResult<()> {
        let handle = self.get(name)?;
        let cancel = {
            let mut g = handle.inner.lock().unwrap();
            match &g.state {
                ProcState::Running { .. }
                | ProcState::Starting
                | ProcState::Stopping
                | ProcState::Restarting { .. } => {
                    return Err(WardenError::InvalidState(
                        name.into(),
                        format!("当前为「{}」无法启动", g.state.name()),
                    ));
                }
                ProcState::Stopped | ProcState::Failed { .. } => {}
            }
            // 清理可能残留的旧 task(防御性)
            if let Some(t) = g.task.take() {
                t.abort();
            }
            if let Some(c) = g.cancel.take() {
                c.cancel();
            }
            let cancel = CancellationToken::new();
            g.cancel = Some(cancel.clone());
            g.restart_count = 0; // 手动 start 重置计数
            g.health = HealthStatus::default(); // 新进程健康状态未知,待 health task 首查
            g.state = ProcState::Starting;
            cancel
        };
        let task = tokio::spawn(proc::supervise(Arc::clone(&handle), cancel));
        handle.inner.lock().unwrap().task = Some(task);
        Ok(())
    }

    /// 停止指定服务(强制 kill)。
    pub async fn stop(&self, name: &str) -> WResult<()> {
        let handle = self.get(name)?;
        handle.shutdown().await
    }

    /// 重启(stop → start)。
    pub async fn restart(&self, name: &str) -> WResult<()> {
        self.stop(name).await?;
        self.start(name).await
    }

    /// 启动所有服务。
    pub async fn start_all(&self) {
        let names: Vec<String> = self.handles.iter().map(|e| e.config.name.clone()).collect();
        for n in names {
            let _ = self.start(&n).await;
        }
    }

    /// 停止所有运行中/重启中的服务。
    pub async fn stop_all(&self) {
        let names: Vec<String> = self.handles.iter().map(|e| e.config.name.clone()).collect();
        for n in names {
            let _ = self.stop(&n).await;
        }
    }

    /// 启动所有 auto_start=true 的服务(daemon 启动时调用)。
    pub async fn start_auto(&self) {
        let names: Vec<String> = self
            .handles
            .iter()
            .filter(|e| e.config.auto_start)
            .map(|e| e.config.name.clone())
            .collect();
        for n in names {
            let _ = self.start(&n).await;
        }
    }

    /// 移除服务(仅 Stopped/Failed 可移除;运行中/重启中返回 InvalidState)。
    pub fn remove(&self, name: &str) -> WResult<()> {
        let handle = self.get(name)?;
        {
            let g = handle.inner.lock().unwrap();
            match &g.state {
                ProcState::Stopped | ProcState::Failed { .. } => {}
                other => {
                    return Err(WardenError::InvalidState(
                        name.into(),
                        format!("当前为「{}」无法删除,请先停止", other.name()),
                    ))
                }
            }
        }
        self.handles.remove(name);
        Ok(())
    }

    /// 更新服务配置(仅 Stopped/Failed 可改;remove + add 语义,重启后生效)。
    pub fn update(&self, config: ServiceConfig) -> WResult<()> {
        let name = config.name.clone();
        self.remove(&name)?;
        self.add(config);
        Ok(())
    }

    // ── desired-state 持久化(daemon 重启后恢复期望状态)──────────────
    // 仅用户显式操作(API start/stop/start-all/stop-all)标记;
    // daemon 优雅停机的 stop_all 不清除,重启后 start_desired 恢复。

    fn desired_path(&self) -> Option<PathBuf> {
        if self.data_dir.as_os_str().is_empty() {
            None
        } else {
            Some(self.data_dir.join("desired_state.json"))
        }
    }

    fn read_desired_map(p: &Path) -> HashMap<String, bool> {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 标记服务的期望运行状态并落盘(data_dir 未配置则跳过)。
    pub fn set_desired(&self, name: &str, running: bool) {
        if let Some(p) = self.desired_path() {
            let mut m = Self::read_desired_map(&p);
            m.insert(name.to_string(), running);
            match serde_json::to_string_pretty(&m) {
                Ok(s) => {
                    if let Err(e) = std::fs::write(&p, s) {
                        tracing::warn!("[supervisor] desired 落盘失败:{e}");
                    }
                }
                Err(e) => tracing::warn!("[supervisor] desired 序列化失败:{e}"),
            }
        }
    }

    /// 启动 desired=true 且当前未运行的服务(daemon 启动时在 start_auto 之后调用)。
    pub async fn start_desired(&self) {
        let Some(p) = self.desired_path() else {
            return;
        };
        for (name, want) in Self::read_desired_map(&p) {
            if !want {
                continue;
            }
            if let Ok(h) = self.get(&name) {
                let running = h.inner.lock().unwrap().state.is_running();
                if !running {
                    tracing::info!("[warden] 恢复期望状态:启动 {name}");
                    let _ = self.start(&name).await;
                }
            }
        }
    }

    pub fn status(&self, name: &str) -> WResult<ServiceStatus> {
        Ok(self.get(name)?.snapshot_status())
    }

    pub fn list(&self) -> Vec<ServiceStatus> {
        self.handles.iter().map(|e| e.snapshot_status()).collect()
    }

    pub fn log_hub(&self, name: &str) -> WResult<Arc<LogHub>> {
        Ok(Arc::clone(&self.get(name)?.log))
    }

    /// 增量同步配置:添加新服务,移除已停止且不在新配置的服务(运行中保留)。
    pub fn apply_config(&self, cfg: &Config) {
        use std::collections::HashSet;
        let new_names: HashSet<String> = cfg.services.iter().map(|s| s.name.clone()).collect();
        self.handles.retain(|name, h| {
            if new_names.contains(name) {
                return true;
            }
            // 不在新配置:仅当非活跃(Stopped/Failed)时移除
            let g = h.inner.lock().unwrap();
            matches!(g.state, ProcState::Stopped | ProcState::Failed { .. })
        });
        for svc in &cfg.services {
            if !self.handles.contains_key(&svc.name) {
                self.add(svc.clone());
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.handles.iter().map(|e| e.config.name.clone()).collect()
    }

    /// 启动后台 metrics 采样 task:周期遍历 Running 服务,按 PID 采 CPU/内存。
    pub fn spawn_metrics(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut sys = sysinfo::System::new();
            loop {
                tokio::time::sleep(interval).await;
                metrics::refresh(&mut sys);
                for entry in self.handles.iter() {
                    let pid = {
                        let g = entry.inner.lock().unwrap();
                        match &g.state {
                            ProcState::Running { pid, .. } => Some(*pid),
                            _ => None,
                        }
                    };
                    if let Some(pid) = pid {
                        if let Some(m) = metrics::sample_one(&sys, pid) {
                            entry.inner.lock().unwrap().metrics = m;
                        }
                    }
                }
            }
        })
    }
}

/// 单个被监护服务的句柄。
pub struct ProcHandle {
    pub config: ServiceConfig,
    pub log: Arc<LogHub>,
    pub(crate) inner: Mutex<ProcInner>,
}

pub(crate) struct ProcInner {
    pub state: ProcState,
    pub restart_count: u32,
    pub last_started_at: Option<DateTime<Utc>>,
    pub metrics: ProcMetrics,
    /// 健康检查结果(health task 周期填充;start 时重置 unknown)。
    pub health: HealthStatus,
    /// 最近一次自然退出(崩溃/正常退出;主动 stop 不记)。
    pub last_exit: Option<LastExit>,
    pub cancel: Option<CancellationToken>,
    pub task: Option<tokio::task::JoinHandle<()>>,
    /// 进程树追踪(Windows Job Object / Unix 进程组):stop 时强杀 + 崩溃保护。
    pub job: Option<crate::supervisor::signal::JobTree>,
}

impl ProcHandle {
    pub fn new(config: ServiceConfig, log: Arc<LogHub>) -> Self {
        Self {
            config,
            log,
            inner: Mutex::new(ProcInner {
                state: ProcState::Stopped,
                restart_count: 0,
                last_started_at: None,
                metrics: ProcMetrics::default(),
                health: HealthStatus::default(),
                last_exit: None,
                cancel: None,
                task: None,
                job: None,
            }),
        }
    }

    pub fn snapshot_status(&self) -> ServiceStatus {
        let g = self.inner.lock().unwrap();
        ServiceStatus {
            name: self.config.name.clone(),
            display_name: self.config.display_name.clone(),
            state: g.state.clone(),
            restart_count: g.restart_count,
            last_started_at: g.last_started_at,
            metrics: g.metrics.clone(),
            health: g.health.clone(),
            last_exit: g.last_exit.clone(),
            auto_start: self.config.auto_start,
            auto_restart: self.config.auto_restart,
        }
    }

    /// 主动停止:设 Stopping → cancel → 等待监护 task 结束。
    pub async fn shutdown(&self) -> WResult<()> {
        let cancel = {
            let g = self.inner.lock().unwrap();
            match &g.state {
                ProcState::Stopped | ProcState::Failed { .. } => return Ok(()),
                _ => g.cancel.clone(),
            }
        };
        if let Some(c) = cancel {
            {
                let mut g = self.inner.lock().unwrap();
                g.state = ProcState::Stopping;
            }
            c.cancel();
            let task = self.inner.lock().unwrap().task.take();
            if let Some(t) = task {
                let _ = t.await;
            }
        }
        Ok(())
    }
}

/// 对外序列化的服务状态快照。
#[derive(Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub display_name: String,
    pub state: ProcState,
    pub restart_count: u32,
    pub last_started_at: Option<DateTime<Utc>>,
    pub metrics: ProcMetrics,
    pub health: HealthStatus,
    pub last_exit: Option<LastExit>,
    pub auto_start: bool,
    pub auto_restart: bool,
}
