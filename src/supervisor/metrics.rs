//! 资源采样:用 sysinfo 按 PID 采 CPU/内存。
//!
//! sysinfo 的 cpu_usage 是自上次 refresh 以来的平均值,因此 metrics loop
//! 周期性 refresh 才能得到有意义的 CPU 数值。

use chrono::Utc;
use sysinfo::{Pid, System};

use crate::model::ProcMetrics;

/// 刷新所有进程信息(供后续 sample_one 读取)。
pub fn refresh(sys: &mut System) {
    sys.refresh_all();
}

/// 采样单个 PID 的 CPU/内存(调用方需先 refresh)。
pub fn sample_one(sys: &System, pid: u32) -> Option<ProcMetrics> {
    let proc = sys.process(Pid::from_u32(pid))?;
    Some(ProcMetrics {
        cpu_percent: proc.cpu_usage(),
        memory_kb: proc.memory() / 1024,
        sampled_at: Some(Utc::now()),
    })
}
