//! 子进程监听端口发现(方案见 docs/PLAN-GROUP-PRIORITY-PORTS.md 需求 2)。
//!
//! 分层:平台采集(netstat2,得 `RawSocketRow` 中性行)→ `filter_listening`
//! 纯过滤(可测核心)→ metrics task 周期写入 `ProcInner.ports`。
//! UDP 只有"已绑定"概念(无 listen 状态),全部保留;TCP 仅保留 LISTEN。

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;

use serde::Serialize;

/// 传输层协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn as_str(&self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// TCP 连接状态(仅区分 LISTEN;其余一律 Other,过滤层不关心细分)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Listen,
    Other,
}

/// 中性 socket 行:平台采集层的输出、过滤层的输入。
#[derive(Clone, Debug)]
pub(crate) struct RawSocketRow {
    pub proto: Proto,
    pub local_addr: IpAddr,
    pub local_port: u16,
    /// TCP 状态;UDP 恒 None。
    pub tcp_state: Option<TcpState>,
    /// 该 socket 关联的 PID(平台表给出;一台 socket 可能关联多个)。
    pub pids: Vec<u32>,
}

/// 对外暴露的监听端口项(ServiceStatus.listening_ports 元素)。
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ListeningSocket {
    /// "tcp" | "udp"
    pub proto: &'static str,
    /// 绑定地址(0.0.0.0/:: 与 127.0.0.1 有安全审计意义,保留原值)。
    pub local_addr: IpAddr,
    pub local_port: u16,
}

/// 从全表 socket 行中筛出 pid_set(服务 PID 子树)监听的端口。
///
/// 规则:TCP 仅保留 LISTEN;UDP 无状态全保留(绑定即报);
/// 输出按 (local_port, proto) 排序,保证快照确定性。
pub(crate) fn filter_listening(
    rows: &[RawSocketRow],
    pid_set: &HashSet<u32>,
) -> Vec<ListeningSocket> {
    let mut out: Vec<ListeningSocket> = rows
        .iter()
        .filter(|r| match r.proto {
            Proto::Tcp => r.tcp_state == Some(TcpState::Listen),
            Proto::Udp => true,
        })
        .filter(|r| r.pids.iter().any(|p| pid_set.contains(p)))
        .map(|r| ListeningSocket {
            proto: r.proto.as_str(),
            local_addr: r.local_addr,
            local_port: r.local_port,
        })
        .collect();
    out.sort_by(|a, b| a.local_port.cmp(&b.local_port).then(a.proto.cmp(b.proto)));
    out
}

// ── 平台采集(netstat2:Windows GetExtendedTcp/UdpTable / Linux netlink)──────

/// 采集全系统 socket 表为中性行。阻塞 OS 调用,调用方需 `spawn_blocking`。
pub(crate) fn collect_rows() -> Result<Vec<RawSocketRow>, String> {
    use netstat2::{
        get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo,
        TcpState as NsState,
    };
    let af = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
    let protos = ProtocolFlags::TCP | ProtocolFlags::UDP;
    let sockets = get_sockets_info(af, protos).map_err(|e| e.to_string())?;
    Ok(sockets
        .into_iter()
        .map(|s| {
            let (proto, tcp_state) = match &s.protocol_socket_info {
                ProtocolSocketInfo::Tcp(t) => (
                    Proto::Tcp,
                    Some(if t.state == NsState::Listen {
                        TcpState::Listen
                    } else {
                        TcpState::Other
                    }),
                ),
                ProtocolSocketInfo::Udp(_) => (Proto::Udp, None),
            };
            RawSocketRow {
                proto,
                local_addr: s.local_addr(),
                local_port: s.local_port(),
                tcp_state,
                pids: s.associated_pids,
            }
        })
        .collect())
}

// ── PID 子树(启动器形态服务:真正监听的是孙进程)──────────────────────

/// 从 sysinfo 全表构建 parent→children 索引(metrics task 每轮一次)。
pub(crate) fn children_index(sys: &sysinfo::System) -> HashMap<u32, Vec<u32>> {
    let mut idx: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, proc) in sys.processes() {
        if let Some(parent) = proc.parent() {
            idx.entry(parent.as_u32()).or_default().push(pid.as_u32());
        }
    }
    idx
}

/// root 及其全部后代(visited 防环)。已知局限:采样间隔内 PID 复用理论上可致
/// 误报,2s 窗口概率可忽略(见 PLAN 文档),不处理。
pub(crate) fn subtree_from(children: &HashMap<u32, Vec<u32>>, root: u32) -> HashSet<u32> {
    let mut set = HashSet::new();
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        if !set.insert(pid) {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn row(proto: Proto, port: u16, state: Option<TcpState>, pids: &[u32]) -> RawSocketRow {
        RawSocketRow {
            proto,
            local_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            local_port: port,
            tcp_state: state,
            pids: pids.to_vec(),
        }
    }

    fn pidset(pids: &[u32]) -> HashSet<u32> {
        pids.iter().copied().collect()
    }

    /// 意图:精确 PID 的 TCP LISTEN 必须可见。
    #[test]
    fn keeps_tcp_listen_for_exact_pid() {
        let rows = vec![
            row(Proto::Tcp, 8790, Some(TcpState::Listen), &[100]),
            row(Proto::Tcp, 8791, Some(TcpState::Listen), &[999]),
        ];
        let out = filter_listening(&rows, &pidset(&[100]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].local_port, 8790);
        assert_eq!(out[0].proto, "tcp");
    }

    /// 意图:出站/已建立连接不是"监听",不得混入。
    #[test]
    fn excludes_non_listen_tcp() {
        let rows = vec![
            row(Proto::Tcp, 40000, Some(TcpState::Other), &[100]), // 客户端出站
            row(Proto::Tcp, 40001, Some(TcpState::Other), &[100]), // ESTABLISHED
            row(Proto::Tcp, 8790, Some(TcpState::Listen), &[100]),
        ];
        let out = filter_listening(&rows, &pidset(&[100]));
        assert_eq!(
            out.iter().map(|s| s.local_port).collect::<Vec<_>>(),
            vec![8790]
        );
    }

    /// 意图:被监护服务是启动器形态时,真正监听的是孙进程——pid_set 含子树全部 PID 即命中。
    #[test]
    fn includes_grandchild_pid() {
        // 100 = 直接子进程,200 = 孙进程(监听者)
        let rows = vec![row(Proto::Tcp, 8787, Some(TcpState::Listen), &[200])];
        let out = filter_listening(&rows, &pidset(&[100, 200]));
        assert_eq!(out.len(), 1, "孙进程的监听端口应可见");
    }

    /// 意图:UDP 无 listen 语义,绑定即报(健康检查不覆盖 UDP,仅展示)。
    #[test]
    fn udp_bound_kept_without_state() {
        let rows = vec![row(Proto::Udp, 6390, None, &[100])];
        let out = filter_listening(&rows, &pidset(&[100]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].proto, "udp");
        assert_eq!(out[0].local_port, 6390);
    }

    /// 意图:其他进程的端口不得串报(误报会误导运维)。
    #[test]
    fn ignores_unrelated_pids() {
        let rows = vec![row(Proto::Udp, 53, None, &[4])]; // 系统 DNS
        assert!(filter_listening(&rows, &pidset(&[100])).is_empty());
    }

    /// 意图:IPv6 监听(devices 常见 :: 绑定)与 IPv4 同等对待。
    #[test]
    fn keeps_ipv6_rows() {
        let rows = vec![RawSocketRow {
            proto: Proto::Tcp,
            local_addr: IpAddr::V6("::".parse().unwrap()),
            local_port: 8080,
            tcp_state: Some(TcpState::Listen),
            pids: vec![100],
        }];
        let out = filter_listening(&rows, &pidset(&[100]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].local_addr.to_string(), "::");
    }

    /// 意图:快照输出确定性(端口序),UI/测试不因表序抖动。
    #[test]
    fn output_sorted_by_port_then_proto() {
        let rows = vec![
            row(Proto::Udp, 9000, None, &[100]),
            row(Proto::Tcp, 8080, Some(TcpState::Listen), &[100]),
            row(Proto::Tcp, 9000, Some(TcpState::Listen), &[100]),
        ];
        let out = filter_listening(&rows, &pidset(&[100]));
        let got: Vec<(u16, &str)> = out.iter().map(|s| (s.local_port, s.proto)).collect();
        assert_eq!(got, vec![(8080, "tcp"), (9000, "tcp"), (9000, "udp")]);
    }

    // ── PID 子树 ──────────────────────────────────────────────────

    fn idx(pairs: &[(u32, u32)]) -> HashMap<u32, Vec<u32>> {
        let mut m: HashMap<u32, Vec<u32>> = HashMap::new();
        for (parent, child) in pairs {
            m.entry(*parent).or_default().push(*child);
        }
        m
    }

    /// 意图:启动器形态(子进程→孙进程链)服务的子树应含整条链。
    #[test]
    fn subtree_includes_descendant_chain() {
        let children = idx(&[(100, 200), (200, 300)]);
        let set = subtree_from(&children, 100);
        assert_eq!(set, pidset(&[100, 200, 300]));
    }

    /// 意图:父链汇合(菱形)与环都不会死循环/重复入集。
    #[test]
    fn subtree_handles_diamond_and_cycle() {
        // 菱形:100→{200,300},200→400,300→400
        let children = idx(&[(100, 200), (100, 300), (200, 400), (300, 400)]);
        assert_eq!(subtree_from(&children, 100), pidset(&[100, 200, 300, 400]));
        // 环:1→2→1
        let cycle = idx(&[(1, 2), (2, 1)]);
        assert_eq!(subtree_from(&cycle, 1), pidset(&[1, 2]));
    }

    /// 意图:子树外的进程(其他服务)不得混入。
    #[test]
    fn subtree_excludes_unrelated() {
        let children = idx(&[(100, 200), (999, 888)]);
        let set = subtree_from(&children, 100);
        assert!(!set.contains(&888) && !set.contains(&999));
    }
}
