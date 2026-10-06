//! 资源采样:按已知 PID 采 CPU/内存,供 metrics loop 周期调用。
//!
//! 平台策略(2026-10 重构,弃用 sysinfo 全表刷新):
//! - **Linux**:直读 `/proc/<pid>/stat`/`statm`(见 [`super::procfs`])。旧
//!   `sys.refresh_all()` 每轮扫描全系统所有进程**及全部线程**,并把每个
//!   `/proc/<pid>/stat` 句柄长期持有、死进程条目永不移除——生产实测 15 天
//!   泄漏 52 万 FD / 7.9 GB RSS,且 CPU 随条目数单调上涨(约 18%)。
//! - **非 Linux(Windows/macOS)**:保留 sysinfo,但改为**定向刷新**已知 PID
//!   (`ProcessesToUpdate::Some`);端口子树索引用**一次性快照**重建(见
//!   [`ProcSampler::children_index`])——sysinfo 各平台实现均为 insert-only,
//!   任何跨轮共享的全表实例都会无限累积死进程条目。残余有界量:定向刷新
//!   实例里服务历史 PID 的条目(每次服务重启 +1 条,几 KB 量级)。
//!
//! CPU% 语义与旧 sysinfo 输出保持一致:`100% = 一核打满`,多核进程可超 100,
//! 上限 = 核数 × 100(`compute_cpu_pct`);首轮无基线时 CPU 为 0、内存照常。

use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;

#[cfg(not(target_os = "linux"))]
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::model::ProcMetrics;

/// CPU% 计算(纯函数,便于单测):`delta_ticks / (CLK_TCK × elapsed_secs) × 100`,
/// 与 sysinfo 的 `Δ(utime+stime) / (全局tickΔ/核数) × 100` 数学等价
/// (全局 tick 每秒恰前进 `核数 × CLK_TCK`)。
pub(crate) fn compute_cpu_pct(
    delta_ticks: u64,
    elapsed_secs: f32,
    clk_tck: u64,
    nb_cpus: u64,
) -> f32 {
    if elapsed_secs <= 0.0 || clk_tck == 0 {
        return 0.0;
    }
    (delta_ticks as f32 / (clk_tck as f32 * elapsed_secs) * 100.0).min(nb_cpus as f32 * 100.0)
}

/// Linux 上一轮采样基线(差分用)。
#[cfg(target_os = "linux")]
struct Prev {
    utime: u64,
    stime: u64,
    starttime: u64,
    at: Instant,
}

/// 按 PID 采样器。单实例由 metrics task 独占,内部状态无并发访问。
pub struct ProcSampler {
    #[cfg(target_os = "linux")]
    prev: HashMap<u32, Prev>,
    #[cfg(target_os = "linux")]
    last: HashMap<u32, ProcMetrics>,
    #[cfg(not(target_os = "linux"))]
    sys: System,
}

impl ProcSampler {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            prev: HashMap::new(),
            #[cfg(target_os = "linux")]
            last: HashMap::new(),
            #[cfg(not(target_os = "linux"))]
            sys: System::new(),
        }
    }

    /// 刷新给定 PID 集(阻塞 IO,量级:Linux 每进程两次小文件读)。
    ///
    /// Linux 下同时完成差分计算并存入 `last`;[`Self::sample`] 仅取结果。
    /// 进程消失的 PID 被静默跳过(基线保留,不影响下次同 PID 新进程的
    /// starttime 复用判定)。
    pub fn refresh(&mut self, pids: &[u32]) {
        #[cfg(target_os = "linux")]
        {
            let now = Instant::now();
            for &pid in pids {
                let Some(stat) = super::procfs::read_stat(pid) else {
                    continue;
                };
                let rss_kb = super::procfs::read_rss_kb(pid).unwrap_or(0);
                let prev = self.prev.get(&pid);
                let cpu = match prev {
                    // starttime 变化 = PID 已被新进程复用,基线作废本轮回 0
                    Some(p) if p.starttime == stat.starttime => compute_cpu_pct(
                        stat.utime.saturating_sub(p.utime) + stat.stime.saturating_sub(p.stime),
                        p.at.elapsed().as_secs_f32(),
                        super::procfs::clock_ticks(),
                        super::procfs::nb_cpus(),
                    ),
                    _ => 0.0,
                };
                self.prev.insert(
                    pid,
                    Prev {
                        utime: stat.utime,
                        stime: stat.stime,
                        starttime: stat.starttime,
                        at: now,
                    },
                );
                self.last.insert(
                    pid,
                    ProcMetrics {
                        cpu_percent: cpu,
                        memory_kb: rss_kb,
                        sampled_at: Some(Utc::now()),
                    },
                );
            }
            // 服务停止/重启(新 PID)后,旧 PID 基线已无意义(同 PID 重来也有
            // starttime 复用守卫),retain 掉——否则崩溃循环服务(每次重启
            // 换 PID)会在两张表里无限累积,恰是本次重构要根除的模式。
            self.prev.retain(|k, _| pids.contains(k));
            self.last.retain(|k, _| pids.contains(k));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let ids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
            self.sys
                .refresh_processes(ProcessesToUpdate::Some(&ids), true);
        }
    }

    /// 采样单个 PID(需先 [`Self::refresh`]);进程不存在返回 `None`。
    pub fn sample(&self, pid: u32) -> Option<ProcMetrics> {
        #[cfg(target_os = "linux")]
        {
            self.last.get(&pid).cloned()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let proc = self.sys.process(Pid::from_u32(pid))?;
            Some(ProcMetrics {
                cpu_percent: proc.cpu_usage(),
                memory_kb: proc.memory() / 1024,
                sampled_at: Some(Utc::now()),
            })
        }
    }

    /// parent→children 索引(端口子树归属用;每轮重建,无跨轮状态)。
    ///
    /// Linux:直扫 `/proc` 只读 ppid(数百进程毫秒级);
    /// 非 Linux:**每轮全新 System 快照、用完即弃**——sysinfo 各平台实现均为
    /// insert-only(windows/system.rs 同样无 retain),共享实例做全表刷新会让
    /// 死进程条目无限累积(Windows 上是 Linux 泄漏问题的同族轻量版)。
    pub fn children_index(&mut self) -> HashMap<u32, Vec<u32>> {
        #[cfg(target_os = "linux")]
        {
            super::procfs::ppid_children_index()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut snapshot = System::new();
            snapshot.refresh_processes(ProcessesToUpdate::All, true);
            let mut idx: HashMap<u32, Vec<u32>> = HashMap::new();
            for (pid, proc) in snapshot.processes() {
                if let Some(parent) = proc.parent() {
                    idx.entry(parent.as_u32()).or_default().push(pid.as_u32());
                }
            }
            idx
        }
    }
}

impl Default for ProcSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_pct_one_core_full() {
        // 1 秒烧满 1 核:delta = CLK_TCK(通常 100)tick → 100%
        assert!((compute_cpu_pct(100, 1.0, 100, 16) - 100.0).abs() < 1e-3);
    }

    #[test]
    fn cpu_pct_half_core() {
        assert!((compute_cpu_pct(50, 1.0, 100, 16) - 50.0).abs() < 1e-3);
    }

    #[test]
    fn cpu_pct_multi_core_exceeds_100() {
        // 2 核打满 → 200%
        assert!((compute_cpu_pct(200, 1.0, 100, 16) - 200.0).abs() < 1e-3);
    }

    #[test]
    fn cpu_pct_capped_at_cpus_times_100() {
        // 4 核全烧但机器只有 2 核(采样噪声)→ 封顶 200
        assert!((compute_cpu_pct(400, 1.0, 100, 2) - 200.0).abs() < 1e-3);
    }

    #[test]
    fn cpu_pct_zero_elapsed_or_clk_is_zero() {
        assert_eq!(compute_cpu_pct(100, 0.0, 100, 16), 0.0);
        assert_eq!(compute_cpu_pct(100, 1.0, 0, 16), 0.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sampler_first_cycle_cpu_zero_then_senses() {
        let pid = std::process::id();
        let mut s = ProcSampler::new();
        s.refresh(&[pid]);
        let first = s.sample(pid).expect("首轮应可采样");
        assert_eq!(
            first.cpu_percent, 0.0,
            "首轮无基线 CPU 为 0(对齐 sysinfo 语义)"
        );
        assert!(first.memory_kb > 0);
        // 烧一点 CPU 再采,delta 可测(结果应为有限非负数)
        let spin = std::time::Instant::now();
        while spin.elapsed().as_millis() < 30 {
            std::hint::black_box(0u64.wrapping_mul(3));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        s.refresh(&[pid]);
        let second = s.sample(pid).expect("次轮应可采样");
        assert!(second.cpu_percent.is_finite() && second.cpu_percent >= 0.0);
        assert!(second.memory_kb > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sampler_pid_reuse_resets_baseline() {
        let pid = std::process::id();
        let mut s = ProcSampler::new();
        s.refresh(&[pid]);
        // 人为把基线 starttime 改成"另一个进程",模拟 PID 复用
        if let Some(p) = s.prev.get_mut(&pid) {
            p.starttime ^= 1;
        }
        s.refresh(&[pid]);
        let m = s.sample(pid).expect("复用后仍应可采样");
        assert_eq!(m.cpu_percent, 0.0, "starttime 变化应重置基线回 0");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sampler_dead_pid_absent() {
        let mut s = ProcSampler::new();
        s.refresh(&[u32::MAX - 1]);
        assert!(
            s.sample(u32::MAX - 1).is_none(),
            "不存在的 PID 采样应返回 None"
        );
    }

    /// 意图:消失的服务 PID(停止/重启换号)不得在采样表里长期滞留——
    /// 否则崩溃循环服务无限累积条目(与 sysinfo 死条目同族)。
    #[cfg(target_os = "linux")]
    #[test]
    fn sampler_prunes_vanished_pids() {
        let pid = std::process::id();
        let mut s = ProcSampler::new();
        s.refresh(&[pid]);
        assert!(s.sample(pid).is_some());
        // 下一轮 pid 集不再包含该 PID(模拟服务已停)→ 两表都应清空它
        s.refresh(&[pid + 1]); // pid+1 几乎必然不存在,仅占位触发 retain
        assert!(s.sample(pid).is_none(), "消失 PID 的缓存应被清理");
        assert!(s.prev.is_empty() && s.last.is_empty());
    }
}
