//! warden 数据模型。
//!
//! 配置层(`ServiceConfig` 等,从 toml 反序列化)与运行态(`ProcState`/`ProcMetrics`)。
//! `ProcRuntime`(含子进程句柄与 LogHub)定义在 `supervisor` 模块,因为它聚合运行时资源。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── 配置层 ────────────────────────────────────────────────────

/// 一个被监护服务的配置。
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ServiceConfig {
    /// 唯一标识(用作 key,文件名等)。
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    /// 可执行文件路径。
    pub command: String,
    /// 命令行参数(数组,优于 serviceMgr-tui 的单字符串)。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// daemon 启动时是否自动拉起。
    #[serde(default)]
    pub auto_start: bool,
    /// 崩溃后是否自动重启(默认 false,按需显式开启)。
    #[serde(default)]
    pub auto_restart: bool,
    /// 重启策略(退避参数)。
    #[serde(default)]
    pub restart: RestartPolicy,
    /// 健康检查(Phase 1 框架定义,Phase 4 完整告警)。
    #[serde(default)]
    pub health: Option<HealthCheck>,
    /// 可在 TUI/Web 打开的管理 URL。
    #[serde(default)]
    pub ui_url: Option<String>,
    /// 子进程配置文件路径(桌面版「编辑」入口;只要求文本格式 toml/ini/yaml 等)。
    /// None = 不提供配置文件编辑。
    #[serde(default)]
    pub config_file: Option<String>,
    /// 优雅停止等待秒数:发信号后给目标 graceful 的时限,超时则强杀整棵进程树。
    #[serde(default = "default_graceful_timeout_secs")]
    pub graceful_timeout_secs: u64,
    /// 子进程 stdout/stderr 文本编码(如 "gbk"/"cp936"/"utf-8");None=UTF-8。
    /// 中文 Windows 控制台程序常输出 GBK,设此项以正确解码(否则中文行会丢失)。
    /// 不支持 UTF-16(其行内字节含 0x0A,无法按行切分)。
    #[serde(default)]
    pub output_encoding: Option<String>,
    /// 分组标签(纯展示/批量操作预留,不参与排序;排序全局由 priority 决定)。
    #[serde(default)]
    pub group: Option<String>,
    /// 启动优先级:数值越小越先启动、越后停止(对齐 supervisord 方向语义);
    /// 同值按 name 字典序。缺省 0。
    #[serde(default)]
    pub priority: u32,
}

fn default_graceful_timeout_secs() -> u64 {
    10
}

/// 崩溃重启的退避策略。
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RestartPolicy {
    /// 重启窗口内最大重试次数,超过则进入 Failed。
    pub max_retries: u32,
    /// 首次退避毫秒。
    pub backoff_initial_ms: u64,
    /// 退避上限毫秒。
    pub backoff_max_ms: u64,
    /// 退避乘数(指数增长)。
    pub backoff_factor: f64,
    /// 重试计数重置窗口秒;距上次启动超过该值则重置计数。
    pub restart_window_secs: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_initial_ms: 1000,
            backoff_max_ms: 60_000,
            backoff_factor: 2.0,
            restart_window_secs: 60,
        }
    }
}

impl RestartPolicy {
    /// 计算第 n 次重试(从 1 开始)前的等待毫秒:`min(initial * factor^(n-1), max)`。
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        if attempt == 0 {
            return self.backoff_initial_ms;
        }
        let mut delay = self.backoff_initial_ms as f64;
        for _ in 0..attempt.saturating_sub(1) {
            delay *= self.backoff_factor;
            if delay >= self.backoff_max_ms as f64 {
                return self.backoff_max_ms;
            }
        }
        delay.round() as u64
    }
}

/// 健康检查(Phase 1 框架,Phase 4 完整实现)。
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HealthCheck {
    Tcp {
        host: String,
        port: u16,
        #[serde(default = "default_tcp_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_health_interval_secs")]
        interval_secs: u64,
    },
}

fn default_tcp_timeout_ms() -> u64 {
    2000
}
fn default_health_interval_secs() -> u64 {
    5
}

// ── 运行态 ────────────────────────────────────────────────────

/// 进程状态机。序列化为 `{ "state": "running", ... }`。
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum ProcState {
    #[default]
    Stopped,
    Starting,
    Running {
        pid: u32,
        started_at: DateTime<Utc>,
    },
    Stopping,
    Failed {
        reason: String,
        exit_code: Option<i32>,
        at: DateTime<Utc>,
    },
    Restarting {
        attempt: u32,
        next_at: DateTime<Utc>,
    },
}

impl ProcState {
    /// 单字状态名,用于列表摘要与过滤。
    pub fn name(&self) -> &'static str {
        match self {
            ProcState::Stopped => "stopped",
            ProcState::Starting => "starting",
            ProcState::Running { .. } => "running",
            ProcState::Stopping => "stopping",
            ProcState::Failed { .. } => "failed",
            ProcState::Restarting { .. } => "restarting",
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, ProcState::Running { .. })
    }
}

/// 资源采样(由 sysinfo 周期填充)。
#[derive(Serialize, Clone, Debug, Default)]
pub struct ProcMetrics {
    pub cpu_percent: f32,
    pub memory_kb: u64,
    pub sampled_at: Option<DateTime<Utc>>,
}

/// 健康检查结果(由 health task 周期填充)。
#[derive(Serialize, Clone, Debug)]
pub struct HealthStatus {
    /// "unknown"(未检查/刚启动)/ "healthy" / "unhealthy"。
    pub status: String,
    pub last_check: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// 连续失败次数(healthy 时为 0)。
    pub consecutive_failures: u32,
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self {
            status: "unknown".into(),
            last_check: None,
            last_error: None,
            consecutive_failures: 0,
        }
    }
}

/// 最近一次进程退出记录(重启后仍保留,供排查)。
#[derive(Serialize, Clone, Debug)]
pub struct LastExit {
    pub exit_code: Option<i32>,
    pub at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let p = RestartPolicy::default(); // initial 1000, factor 2.0, max 60000
        assert_eq!(p.backoff_ms(1), 1000);
        assert_eq!(p.backoff_ms(2), 2000);
        assert_eq!(p.backoff_ms(3), 4000);
        assert_eq!(p.backoff_ms(4), 8000);
        // 第 7 次:1000*2^6=64000 → 封顶 60000
        assert_eq!(p.backoff_ms(7), 60000);
        assert_eq!(p.backoff_ms(20), 60000);
    }

    #[test]
    fn proc_state_name_and_running() {
        assert_eq!(ProcState::Stopped.name(), "stopped");
        assert!(!ProcState::Stopped.is_running());
        let running = ProcState::Running {
            pid: 1,
            started_at: Utc::now(),
        };
        assert!(running.is_running());
        assert_eq!(running.name(), "running");
    }
}
