//! 有序启停 e2e:按 priority 顺序启动、逆序停止(方案见 docs/PLAN-GROUP-PRIORITY-PORTS.md)。
//!
//! 验证意图:被依赖的服务(priority 小)先就绪、最后停止;单个服务故障不阻塞整组拉起。

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use warden::config::Config;
use warden::model::ServiceConfig;
use warden::supervisor::Supervisor;

const STAMP_TARGET: &str = env!("CARGO_BIN_EXE_stamp_target");

fn stamp_svc(name: &str, priority: u32, stamp: &std::path::Path) -> ServiceConfig {
    ServiceConfig {
        name: name.into(),
        display_name: String::new(),
        description: String::new(),
        command: STAMP_TARGET.into(),
        args: vec![name.into(), stamp.to_string_lossy().into()],
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
        priority,
    }
}

fn fail_svc(name: &str, priority: u32) -> ServiceConfig {
    let (cmd, args) = common::quick_fail();
    ServiceConfig {
        name: name.into(),
        display_name: String::new(),
        description: String::new(),
        command: cmd,
        args,
        working_dir: None,
        environment: HashMap::new(),
        auto_start: false,
        auto_restart: false,
        restart: Default::default(),
        health: None,
        ui_url: None,
        graceful_timeout_secs: 1,
        output_encoding: None,
        group: None,
        priority,
    }
}

fn supervisor_with(svcs: Vec<ServiceConfig>) -> Supervisor {
    let cfg = Config {
        services: svcs,
        ..Default::default()
    };
    Supervisor::from_config(&cfg, PathBuf::from(""))
}

fn read_stamps(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

fn stamp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("warden-order-{}-{}.stamp", tag, std::process::id()))
}

/// 轮询直到服务进入目标状态或超时(对齐 supervisor_e2e 的 wait_for_state)。
async fn wait_for_state(sv: &Supervisor, name: &str, want: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = sv.status(name) {
            if s.state.name() == want {
                return;
            }
        }
        if Instant::now() >= deadline {
            let cur = sv
                .status(name)
                .map(|s| s.state.name().to_string())
                .unwrap_or_default();
            panic!("等待 {name} 进入「{want}」超时,当前为「{cur}」");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 意图:按优先级小→大启动,启动序应体现在进程实际执行序(stamp 行序)上。
/// 服务名故意与 priority 序相反(alpha 字典序最小但 priority 最大),证明按 priority 而非 name 排序。
#[tokio::test]
async fn start_all_respects_priority_order() {
    let stamp = stamp_path("start");
    let _ = std::fs::remove_file(&stamp);
    // priority 序:beta(0) → gamma(5) → alpha(10)
    let sv = supervisor_with(vec![
        stamp_svc("alpha", 10, &stamp),
        stamp_svc("beta", 0, &stamp),
        stamp_svc("gamma", 5, &stamp),
    ]);

    sv.start_all().await;
    for n in ["alpha", "beta", "gamma"] {
        wait_for_state(&sv, n, "running", Duration::from_secs(10)).await;
    }

    let stamps = read_stamps(&stamp);
    let starts: Vec<&str> = stamps.iter().map(String::as_str).collect();
    assert_eq!(
        starts,
        vec!["start:beta", "start:gamma", "start:alpha"],
        "启动 stamp 行序应为优先级序,实际 {starts:?}"
    );
    let _ = std::fs::remove_file(&stamp);
}

/// 意图:start_all 逐个等待前序就绪(就绪推进)——返回时所有服务应已离开 Starting。
/// 这是有序启动的行为保障:后序服务的 start 发起必然晚于前序服务的进程创建。
#[tokio::test]
async fn start_all_waits_for_each_service_ready() {
    let stamp = stamp_path("ready");
    let _ = std::fs::remove_file(&stamp);
    let sv = supervisor_with(vec![
        stamp_svc("alpha", 10, &stamp),
        stamp_svc("beta", 0, &stamp),
    ]);

    sv.start_all().await;
    // 不轮询、立即断言:start_all 返回即应全部就绪(而非停留在 starting)
    for n in ["alpha", "beta"] {
        let state = sv.status(n).unwrap().state.name().to_string();
        assert_eq!(state, "running", "start_all 返回后 {n} 应已就绪");
    }
    let _ = std::fs::remove_file(&stamp);
}

/// 意图:停止逆序——被依赖方(priority 小)最后停,stop stamp 行序为启动序的逆序。
#[tokio::test]
async fn stop_all_stops_in_reverse_order() {
    let stamp = stamp_path("stop");
    let _ = std::fs::remove_file(&stamp);
    let sv = supervisor_with(vec![
        stamp_svc("alpha", 10, &stamp),
        stamp_svc("beta", 0, &stamp),
        stamp_svc("gamma", 5, &stamp),
    ]);

    sv.start_all().await;
    for n in ["alpha", "beta", "gamma"] {
        wait_for_state(&sv, n, "running", Duration::from_secs(10)).await;
    }
    sv.stop_all().await;

    let all = read_stamps(&stamp);
    let stops: Vec<&str> = all
        .iter()
        .filter(|l| l.starts_with("stop:"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        stops,
        vec!["stop:alpha", "stop:gamma", "stop:beta"],
        "停止 stamp 行序应为启动序逆序,实际 {stops:?}"
    );
    let _ = std::fs::remove_file(&stamp);
}

/// 意图:最高优先级服务启动即失败(速退熔断),不阻塞低优先级服务拉起。
#[tokio::test]
async fn start_all_continues_after_failure() {
    let stamp = stamp_path("fail");
    let _ = std::fs::remove_file(&stamp);
    let sv = supervisor_with(vec![fail_svc("failer", 0), stamp_svc("slow", 10, &stamp)]);

    sv.start_all().await;
    wait_for_state(&sv, "failer", "failed", Duration::from_secs(10)).await;
    wait_for_state(&sv, "slow", "running", Duration::from_secs(10)).await;
    assert_eq!(read_stamps(&stamp), vec!["start:slow"]);
    let _ = std::fs::remove_file(&stamp);
}
