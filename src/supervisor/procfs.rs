//! Linux /proc 最小读取层(仅 `target_os = "linux"`)。
//!
//! 存在的动机:sysinfo 0.32 在 Linux 上按"全系统进程数据库"设计——每个被刷新
//! 进程的 `/proc/<pid>/stat` 句柄被长期持有,且死进程条目永不移除,长期运行会
//! 同时泄漏 FD 与内存(实测 15 天 52 万 FD / 7.9 GB,见 ROADMAP 变更日志)。
//! warden 只需要"少量已知 PID 的 CPU/内存 + 一张 ppid 表",这里用**一次性
//! open→read→close** 的直读实现:调用间零共享状态、零 FD 滞留、零条目积累。
//!
//! 所有读取都是阻塞文件 IO(单文件 <1 KB,量级微秒到毫秒);metrics task 的
//! 调用频率(秒级)下成本可忽略,沿用旧 sysinfo 同步调用形态,不额外
//! spawn_blocking。

use std::collections::HashMap;
use std::fs;
use std::sync::OnceLock;

/// `/proc/<pid>/stat` 中 warden 关心的字段(字段号按 proc_pid_stat(5),从 1 计)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcStat {
    /// 父进程号(字段 4)。
    pub ppid: u32,
    /// 用户态 CPU tick(字段 14)。
    pub utime: u64,
    /// 内核态 CPU tick(字段 15)。
    pub stime: u64,
    /// 进程启动时刻(自开机起的 tick 数,字段 22)——PID 复用判定锚点。
    pub starttime: u64,
}

/// CLK_TCK(每秒时钟 tick 数),进程级 sysconf 缓存。
pub(crate) fn clock_ticks() -> u64 {
    static CLK: OnceLock<u64> = OnceLock::new();
    *CLK.get_or_init(|| {
        // SAFETY: sysconf 查询只读常量,无并发/句柄风险;Linux 上恒为 100,
        // 失败路径(-1/0)由 max(1) 兜底(仅作除零防护,不影响除法正确性量级)。
        unsafe { libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64 }
    })
}

/// 页大小(字节),进程级 sysconf 缓存。
fn page_size() -> u64 {
    static PAGE: OnceLock<u64> = OnceLock::new();
    *PAGE.get_or_init(|| unsafe {
        // SAFETY: 同 clock_ticks,只读常量查询。
        libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64
    })
}

/// 逻辑 CPU 数(封顶用,与 sysinfo 旧语义一致:CPU% 上限 = 核数 × 100)。
pub(crate) fn nb_cpus() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
}

/// 解析 `/proc/<pid>/stat` 内容为 [`ProcStat`]。
///
/// comm(字段 2)是括号包裹的进程名,可含空格与括号,因此定位**最后一个** `)`
/// 之后再按空白切分;其后的 token[0] 即字段 3(state),token[N-3] 为字段 N。
/// 字段不足或非数字返回 `None`(调用方按进程已退出处理)。
pub(crate) fn parse_stat(content: &str) -> Option<ProcStat> {
    let close = content.rfind(')')?;
    let mut it = content[close + 1..].split_whitespace();
    // 依次消费字段 3..=22 中需要的下标:state(3) ppid(4) .. utime(14) stime(15) .. starttime(22)
    let mut fields: [Option<&str>; 20] = [None; 20]; // 下标 0..=19 对应字段 3..=22
    for slot in &mut fields {
        *slot = it.next();
    }
    let field = |n: usize| fields.get(n.wrapping_sub(3)).copied().flatten();
    Some(ProcStat {
        ppid: field(4)?.parse().ok()?,
        utime: field(14)?.parse().ok()?,
        stime: field(15)?.parse().ok()?,
        starttime: field(22)?.parse().ok()?,
    })
}

/// 读取单个进程的 stat(一次 open→read→close,FD 瞬时)。
pub(crate) fn read_stat(pid: u32) -> Option<ProcStat> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&content)
}

/// 常驻内存 KB(`/proc/<pid>/statm` 字段 2 × 页大小 ÷ 1024;与 sysinfo 的
/// `Process::memory()` 同源——它也是读 statm 的 resident 页)。
pub(crate) fn read_rss_kb(pid: u32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident: u64 = content.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident * page_size() / 1024)
}

/// 扫一遍 `/proc` 数值目录,构建 parent→children 索引(端口子树归属用)。
///
/// 每轮重建、用完即弃:无跨轮状态即无积累。~数百进程的单遍 stat 读取为
/// 毫秒级,显著低于旧 sysinfo 全表刷新(其还要读 cmd/environ/全部线程)。
pub(crate) fn ppid_children_index() -> HashMap<u32, Vec<u32>> {
    let mut idx: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(dir) = fs::read_dir("/proc") else {
        return idx;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(stat) = read_stat(pid) {
            idx.entry(stat.ppid).or_default().push(pid);
        }
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_basic() {
        // 真实形态:pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt
        // majflt cmajflt utime stime cutime cstime priority nice threads itreal starttime ...
        let line = "4093 (warden) S 1680 4093 4093 34816 4093 4194304 1200 0 0 0 \
                    12345 6789 0 0 20 0 36 0 987654 9629248 1994523 ...";
        let st = parse_stat(line).expect("应解析成功");
        assert_eq!(st.ppid, 1680);
        assert_eq!(st.utime, 12345);
        assert_eq!(st.stime, 6789);
        assert_eq!(st.starttime, 987654);
    }

    #[test]
    fn parse_stat_comm_with_spaces_and_parens() {
        // comm 可含空格与括号(如 node 的 "(MainThread)");取最后一个 ')' 后切分
        let line = "1524573 (vite worker (2)) R 4093 1 0 0 -1 4194304 500 0 0 0 \
                    10 5 0 0 20 0 12 0 111 0 0";
        let st = parse_stat(line).expect("含括号 comm 应解析成功");
        assert_eq!(st.ppid, 4093);
        assert_eq!(st.utime, 10);
        assert_eq!(st.stime, 5);
        assert_eq!(st.starttime, 111);
    }

    #[test]
    fn parse_stat_truncated_returns_none() {
        assert!(parse_stat("1 (init) S 0").is_none(), "字段不足应返回 None");
        assert!(parse_stat("").is_none());
    }

    #[test]
    fn read_stat_of_self() {
        let pid = std::process::id();
        let st = read_stat(pid).expect("自身 stat 应可读");
        assert!(st.starttime > 0, "starttime 必为正(自开机 tick)");
        assert!(st.ppid > 0);
    }

    #[test]
    fn read_rss_kb_of_self() {
        let kb = read_rss_kb(std::process::id()).expect("自身 statm 应可读");
        assert!(kb > 0, "测试进程常驻内存必为正");
    }

    #[test]
    fn read_stat_of_dead_pid_returns_none() {
        // 内核线程 pid 0 无 /proc/0/stat;用不存在的巨大 PID 更稳
        assert!(read_stat(u32::MAX - 1).is_none());
    }

    #[test]
    fn ppid_children_index_contains_self() {
        let idx = ppid_children_index();
        let pid = std::process::id();
        let ppid = read_stat(pid).expect("self stat").ppid;
        assert!(
            idx.get(&ppid).is_some_and(|kids| kids.contains(&pid)),
            "自身应出现在父进程的 children 列表"
        );
    }

    #[test]
    fn clock_ticks_sane() {
        assert!(clock_ticks() >= 1);
        assert!(page_size() >= 4096);
    }
}
