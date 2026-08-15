//! 端口发现 e2e(方案见 docs/PLAN-GROUP-PRIORITY-PORTS.md 需求 2)。
//!
//! 验证意图:被监护服务实际监听的 TCP/UDP 端口能被 warden 看见并进入状态快照。
//! 断言用双信源一致:helper 自报端口(stderr 行)vs OS 端口表(status.listening_ports)。

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use warden::config::Config;
use warden::model::ServiceConfig;
use warden::supervisor::Supervisor;

const PORT_TARGET: &str = env!("CARGO_BIN_EXE_port_listener_target");

fn listener_svc(name: &str, args: Vec<String>) -> ServiceConfig {
    ServiceConfig {
        name: name.into(),
        display_name: String::new(),
        description: String::new(),
        command: PORT_TARGET.into(),
        args,
        working_dir: None,
        environment: HashMap::new(),
        auto_start: false,
        auto_restart: false,
        restart: Default::default(),
        health: None,
        ui_url: None,
        graceful_timeout_secs: 5,
        output_encoding: None,
        group: None,
        priority: 0,
    }
}

/// 意图:直接子进程监听的 TCP/UDP 端口应出现在 listening_ports。
#[tokio::test]
async fn discovers_listener_ports() {
    let sv = std::sync::Arc::new(setup(vec![listener_svc("listener", vec![])]));
    sv.start("listener").await.unwrap();
    wait_running(&sv, "listener").await;

    let expected = wait_self_reported(&sv, "listener", 2).await; // tcp + udp 各 1
    let got = wait_ports(&sv, "listener", &expected).await;
    assert!(got, "应发现自报端口 {expected:?}");

    sv.stop_all().await;
}

/// 意图:启动器形态(真正监听的是孙进程)服务,孙进程端口同样可见。
#[tokio::test]
async fn discovers_grandchild_ports() {
    let sv = std::sync::Arc::new(setup(vec![listener_svc(
        "launcher",
        vec!["--grandchild".into()],
    )]));
    sv.start("launcher").await.unwrap();
    wait_running(&sv, "launcher").await;

    // 直接子进程 tcp/udp 各 1 + 孙进程 tcp/udp 各 1
    let expected = wait_self_reported(&sv, "launcher", 4).await;
    let got = wait_ports(&sv, "launcher", &expected).await;
    assert!(got, "孙进程端口也应发现,自报 {expected:?}");

    sv.stop_all().await;
}

// ── 测试基建 ────────────────────────────────────────────────────

fn setup(svcs: Vec<ServiceConfig>) -> std::sync::Arc<Supervisor> {
    let cfg = Config {
        services: svcs,
        ..Default::default()
    };
    let sv = std::sync::Arc::new(Supervisor::from_config(&cfg, PathBuf::from("")));
    // 端口刷新挂在 metrics task(与生产同路径);300ms 加速采样
    let _metrics_task = std::sync::Arc::clone(&sv).spawn_metrics(Duration::from_millis(300));
    sv
}

async fn wait_running(sv: &Supervisor, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = sv.status(name) {
            if s.state.is_running() {
                return;
            }
        }
        assert!(Instant::now() < deadline, "{name} 未进入 running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 从 LogHub 提取 helper 自报的 `PORT <proto> <addr> <port>` 行,凑满 want 条。
async fn wait_self_reported(
    sv: &Supervisor,
    name: &str,
    want: usize,
) -> Vec<(String, String, u16)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ports: Vec<(String, String, u16)> = sv
            .log_hub(name)
            .unwrap()
            .snapshot(200)
            .iter()
            .filter_map(|l| {
                let p: Vec<&str> = l.text.split_whitespace().collect();
                match p.as_slice() {
                    ["PORT", proto, addr, port] => Some((
                        (*proto).to_string(),
                        (*addr).to_string(),
                        port.parse().ok()?,
                    )),
                    _ => None,
                }
            })
            .collect();
        if ports.len() >= want {
            return ports;
        }
        assert!(Instant::now() < deadline, "{name} 自报端口行不足 {want} 条");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 轮询 listening_ports 直到覆盖全部期望项(端口发现与启动有几秒延迟)。
async fn wait_ports(sv: &Supervisor, name: &str, expected: &[(String, String, u16)]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snap = sv.status(name).unwrap();
        let all: Vec<(String, String, u16)> = snap
            .listening_ports
            .iter()
            .map(|s| (s.proto.to_string(), s.local_addr.to_string(), s.local_port))
            .collect();
        if expected.iter().all(|e| all.contains(e)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
