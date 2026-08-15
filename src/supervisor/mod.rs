//! 监护引擎:管理一组被监护服务的生命周期。
//!
//! 每个服务对应一个 `ProcHandle`;`Supervisor::start` 后 spawn 一个
//! `proc::supervise` task,在该 task 内独占子进程、接管 stdout/stderr、
//! 监听退出并按 `RestartPolicy` 决策重启。`stop` 通过 `CancellationToken`
//! 通知 task 主动终止(强制 kill)。设计见 docs/DESIGN.md §6。

pub mod health;
pub mod metrics;
pub mod ports;
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

pub use ports::ListeningSocket;

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

    /// 按启动顺序返回服务名(priority 升序,同值按 name 字典序);
    /// reverse=true 为停止顺序(启动序的逆序,被依赖方最后停)。
    pub fn ordered_names(&self, reverse: bool) -> Vec<String> {
        let mut names: Vec<(String, u32)> = self
            .handles
            .iter()
            .map(|e| {
                let g = e.inner.lock().unwrap();
                (g.config.name.clone(), g.config.priority)
            })
            .collect();
        names.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if reverse {
            names.reverse();
        }
        names.into_iter().map(|(n, _)| n).collect()
    }

    /// 按启动/停止顺序返回指定组的服务名(组不存在返回空)。
    pub fn names_in_group(&self, reverse: bool, group: &str) -> Vec<String> {
        self.ordered_names(reverse)
            .into_iter()
            .filter(|n| {
                self.get(n)
                    .map(|h| h.inner.lock().unwrap().config.group.as_deref() == Some(group))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// 启动指定组的全部服务(按优先级顺序 + 就绪推进),返回实际启动的服务名。
    pub async fn start_group(&self, group: &str) -> Vec<String> {
        let names = self.names_in_group(false, group);
        for n in &names {
            let _ = self.start(n).await;
            self.wait_started(n).await;
        }
        names
    }

    /// 停止指定组的全部服务(逆序,被依赖方最后停),返回实际停止的服务名。
    pub async fn stop_group(&self, group: &str) -> Vec<String> {
        let names = self.names_in_group(true, group);
        for n in &names {
            let _ = self.stop(n).await;
        }
        names
    }

    /// 启动所有服务(按优先级顺序 + 就绪推进)。
    pub async fn start_all(&self) {
        self.start_ordered(|_| true).await;
    }

    /// 停止所有运行中/重启中的服务(启动序的逆序,被依赖方最后停)。
    pub async fn stop_all(&self) {
        for n in self.ordered_names(true) {
            let _ = self.stop(&n).await;
        }
    }

    /// 启动所有 auto_start=true 的服务(daemon 启动时调用,按优先级顺序 + 就绪推进)。
    pub async fn start_auto(&self) {
        self.start_ordered(|h| h.inner.lock().unwrap().config.auto_start)
            .await;
    }

    /// 就绪推进的等待目标:前序服务进入终态/稳态(Running/Failed/Restarting/Stopped)
    /// 或超时后,才发起下一个服务,使被依赖方(小 priority)先就绪;
    /// 单个服务异常不阻塞整组拉起。慢启动服务超时被跳过仅指不再等待,不取消启动。
    const START_READY_TIMEOUT: Duration = Duration::from_secs(15);

    /// 按启动顺序逐个启动(过滤谓词筛选参与的服务),每个就绪推进后再启动下一个。
    async fn start_ordered(&self, include: impl Fn(&ProcHandle) -> bool) {
        for n in self.ordered_names(false) {
            let Ok(h) = self.get(&n) else {
                continue;
            };
            if !include(&h) {
                continue;
            }
            let _ = self.start(&n).await;
            self.wait_started(&n).await;
        }
    }

    /// 等待服务离开 Starting(就绪/终态)或超时兜底返回。
    async fn wait_started(&self, name: &str) {
        let deadline = tokio::time::Instant::now() + Self::START_READY_TIMEOUT;
        loop {
            let settled = match self.get(name) {
                Ok(h) => {
                    let g = h.inner.lock().unwrap();
                    !matches!(g.state, ProcState::Starting)
                }
                Err(_) => true,
            };
            if settled || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
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

    /// 更新服务配置:运行中也允许保存(保留进程状态与日志句柄,
    /// 新配置下次启动生效;health/组排序等读 config 的即时生效)。
    /// 删除仍要求停止(remove)。name 必须已存在(ServiceNotFound)。
    pub fn update(&self, config: ServiceConfig) -> WResult<()> {
        let handle = self.get(&config.name)?;
        handle.inner.lock().unwrap().config = config;
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

    /// 启动 desired=true 且当前未运行的服务(daemon 启动时在 start_auto 之后调用,
    /// 按优先级顺序 + 就绪推进,恢复语义与手动 start_all 一致)。
    pub async fn start_desired(&self) {
        let Some(p) = self.desired_path() else {
            return;
        };
        let desired = Self::read_desired_map(&p);
        self.start_ordered(|h| {
            let name = h.inner.lock().unwrap().config.name.clone();
            desired.get(&name).copied().unwrap_or(false)
        })
        .await;
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
        self.handles
            .iter()
            .map(|e| e.inner.lock().unwrap().config.name.clone())
            .collect()
    }

    /// 启动后台 metrics 采样 task:周期遍历 Running 服务,按 PID 采 CPU/内存,
    /// 并刷新监听端口(服务 PID 子树内实际 LISTEN/绑定的 TCP/UDP,含孙进程)。
    pub fn spawn_metrics(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut sys = sysinfo::System::new();
            loop {
                tokio::time::sleep(interval).await;
                metrics::refresh(&mut sys);
                let index = ports::children_index(&sys);
                // 全表 socket 采集一次供全部服务共享;阻塞 OS 调用放 spawn_blocking
                let rows = match tokio::task::spawn_blocking(ports::collect_rows).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        tracing::warn!("[metrics] 端口表采集失败:{e}");
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("[metrics] 端口采集任务 join 失败:{e}");
                        Vec::new()
                    }
                };
                for entry in self.handles.iter() {
                    let pid = {
                        let g = entry.inner.lock().unwrap();
                        match &g.state {
                            ProcState::Running { pid, .. } => Some(*pid),
                            _ => None,
                        }
                    };
                    if let Some(pid) = pid {
                        let mut g = entry.inner.lock().unwrap();
                        if let Some(m) = metrics::sample_one(&sys, pid) {
                            g.metrics = m;
                        }
                        let set = ports::subtree_from(&index, pid);
                        g.ports = ports::filter_listening(&rows, &set);
                    }
                }
            }
        })
    }
}

/// 单个被监护服务的句柄。
pub struct ProcHandle {
    pub log: Arc<LogHub>,
    pub(crate) inner: Mutex<ProcInner>,
}

pub(crate) struct ProcInner {
    /// 当前配置(update 可运行中替换;supervise 每次重启取新快照生效)。
    pub config: ServiceConfig,
    pub state: ProcState,
    pub restart_count: u32,
    pub last_started_at: Option<DateTime<Utc>>,
    pub metrics: ProcMetrics,
    /// 健康检查结果(health task 周期填充;start 时重置 unknown)。
    pub health: HealthStatus,
    /// 最近一次自然退出(崩溃/正常退出;主动 stop 不记)。
    pub last_exit: Option<LastExit>,
    /// 监听端口快照(metrics task 周期刷新;服务 PID 子树内,含孙进程)。
    pub ports: Vec<ListeningSocket>,
    pub cancel: Option<CancellationToken>,
    pub task: Option<tokio::task::JoinHandle<()>>,
    /// 进程树追踪(Windows Job Object / Unix 进程组):stop 时强杀 + 崩溃保护。
    pub job: Option<crate::supervisor::signal::JobTree>,
}

impl ProcHandle {
    pub fn new(config: ServiceConfig, log: Arc<LogHub>) -> Self {
        Self {
            log,
            inner: Mutex::new(ProcInner {
                config,
                state: ProcState::Stopped,
                restart_count: 0,
                last_started_at: None,
                metrics: ProcMetrics::default(),
                health: HealthStatus::default(),
                last_exit: None,
                ports: Vec::new(),
                cancel: None,
                task: None,
                job: None,
            }),
        }
    }

    pub fn snapshot_status(&self) -> ServiceStatus {
        let g = self.inner.lock().unwrap();
        ServiceStatus {
            name: g.config.name.clone(),
            display_name: g.config.display_name.clone(),
            state: g.state.clone(),
            restart_count: g.restart_count,
            last_started_at: g.last_started_at,
            metrics: g.metrics.clone(),
            health: g.health.clone(),
            last_exit: g.last_exit.clone(),
            listening_ports: g.ports.clone(),
            auto_start: g.config.auto_start,
            auto_restart: g.config.auto_restart,
            group: g.config.group.clone(),
            priority: g.config.priority,
            ui_url: g.config.ui_url.clone(),
            config_file: g.config.config_file.clone(),
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
    /// 监听端口(TCP LISTEN / UDP 绑定;服务 PID 子树,含孙进程)。
    pub listening_ports: Vec<ListeningSocket>,
    pub auto_start: bool,
    pub auto_restart: bool,
    /// 分组标签(纯展示)。
    pub group: Option<String>,
    /// 启动优先级(小者先启动、后停止)。
    pub priority: u32,
    /// UI 入口(桌面版「打开」按钮;None = 禁用)。
    pub ui_url: Option<String>,
    /// 配置文件路径(桌面版「编辑」按钮;None = 禁用)。
    pub config_file: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, priority: u32) -> ServiceConfig {
        ServiceConfig {
            name: name.into(),
            display_name: String::new(),
            description: String::new(),
            command: "true".into(),
            args: vec![],
            working_dir: None,
            environment: HashMap::new(),
            auto_start: false,
            auto_restart: false,
            restart: Default::default(),
            health: None,
            ui_url: None,
            config_file: None,
            graceful_timeout_secs: 1,
            output_encoding: None,
            group: None,
            priority,
        }
    }

    #[test]
    fn start_order_sorts_by_priority_then_name() {
        let sv = Supervisor::new(PathBuf::from(""));
        sv.add(svc("a", 20));
        sv.add(svc("b", 10));
        sv.add(svc("c", 10)); // 与 b 同 priority,按 name 字典序
        assert_eq!(sv.ordered_names(false), vec!["b", "c", "a"]);
    }

    #[test]
    fn stop_order_is_reverse_of_start_order() {
        let sv = Supervisor::new(PathBuf::from(""));
        sv.add(svc("a", 20));
        sv.add(svc("b", 10));
        sv.add(svc("c", 10));
        assert_eq!(sv.ordered_names(true), vec!["a", "c", "b"]);
    }

    fn svc_in_group(name: &str, priority: u32, group: Option<&str>) -> ServiceConfig {
        ServiceConfig {
            group: group.map(String::from),
            ..svc(name, priority)
        }
    }

    /// 意图:组级启停只作用于该组,且组内仍按优先级序(启动正序/停止逆序)。
    #[test]
    fn group_filter_keeps_priority_order_within_group() {
        let sv = Supervisor::new(PathBuf::from(""));
        sv.add(svc_in_group("a", 20, Some("g1")));
        sv.add(svc_in_group("b", 10, Some("g2")));
        sv.add(svc_in_group("c", 10, Some("g1")));
        sv.add(svc_in_group("d", 0, None)); // 未分组,不属于任何组
        assert_eq!(sv.names_in_group(false, "g1"), vec!["c", "a"]);
        assert_eq!(sv.names_in_group(true, "g1"), vec!["a", "c"]);
        assert!(sv.names_in_group(false, "no-such").is_empty());
    }
}
