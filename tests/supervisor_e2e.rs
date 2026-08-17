//! 监护引擎端到端测试:用真实无害进程验证状态机、重启、熔断。

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use warden::config::Config;
use warden::model::{RestartPolicy, ServiceConfig};
use warden::supervisor::Supervisor;
use warden::WardenError;

fn make_config(
    name: &str,
    cmd: &str,
    args: Vec<String>,
    auto_restart: bool,
    max_retries: u32,
) -> ServiceConfig {
    ServiceConfig {
        name: name.into(),
        display_name: String::new(),
        description: String::new(),
        command: cmd.into(),
        args,
        working_dir: None,
        environment: HashMap::new(),
        auto_start: false,
        auto_restart,
        restart: RestartPolicy {
            max_retries,
            backoff_initial_ms: 100,
            backoff_max_ms: 500,
            backoff_factor: 2.0,
            restart_window_secs: 3600,
        },
        health: None,
        ui_url: None,
        config_file: None,
        graceful_timeout_secs: 1,
        output_encoding: None,
        group: None,
        priority: 0,
    }
}

fn supervisor_with(svc: ServiceConfig) -> Supervisor {
    let cfg = Config {
        services: vec![svc],
        ..Default::default()
    };
    Supervisor::from_config(&cfg, PathBuf::from(""))
}

/// 轮询直到服务进入目标状态或超时。
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

#[tokio::test]
async fn start_long_running_then_stop() {
    let (cmd, args) = common::long_runner();
    let sv = supervisor_with(make_config("longrun", &cmd, args, false, 3));

    sv.start("longrun").await.unwrap();
    wait_for_state(&sv, "longrun", "running", Duration::from_secs(5)).await;

    sv.stop("longrun").await.unwrap();
    wait_for_state(&sv, "longrun", "stopped", Duration::from_secs(3)).await;
    assert_eq!(sv.status("longrun").unwrap().state.name(), "stopped");
}

#[tokio::test]
async fn quick_exit_without_restart_becomes_failed() {
    let (cmd, args) = common::quick_fail();
    let sv = supervisor_with(make_config("quick", &cmd, args, false, 3));

    sv.start("quick").await.unwrap();
    wait_for_state(&sv, "quick", "failed", Duration::from_secs(3)).await;
    assert_eq!(sv.status("quick").unwrap().state.name(), "failed");
}

#[tokio::test]
async fn quick_exit_with_restart_hits_limit() {
    let (cmd, args) = common::quick_fail();
    // max_retries=2:重试 2 次(退避 100+200ms),第 3 次退出时熔断
    let sv = supervisor_with(make_config("retry", &cmd, args, true, 2));

    sv.start("retry").await.unwrap();
    wait_for_state(&sv, "retry", "failed", Duration::from_secs(5)).await;

    let st = sv.status("retry").unwrap();
    assert_eq!(st.state.name(), "failed");
    assert_eq!(st.restart_count, 2, "应在重试 2 次后熔断");
}

/// 意图:command 不存在时 spawn 失败 → Failed(带原因);且零重试——
/// auto_restart=true 也不进重启决策(坏路径重试无意义),restart_count 保持 0。
#[tokio::test]
async fn start_nonexistent_command_fails_without_retry() {
    let sv = supervisor_with(make_config("ghost", "warden-no-such-cmd", vec![], true, 3));

    sv.start("ghost").await.unwrap();
    wait_for_state(&sv, "ghost", "failed", Duration::from_secs(5)).await;

    let st = sv.status("ghost").unwrap();
    assert_eq!(st.restart_count, 0, "spawn 失败不进重启决策(零重试)");
    match st.state {
        warden::model::ProcState::Failed { reason, .. } => {
            assert!(
                reason.contains("spawn 失败"),
                "原因应含 'spawn 失败':{reason}"
            );
        }
        other => panic!("应为 Failed,实际:{other:?}"),
    }
}

#[tokio::test]
async fn start_unknown_service_returns_not_found() {
    let sv = Supervisor::new(PathBuf::from(""));
    let err = sv.start("nope").await.unwrap_err();
    assert!(
        matches!(err, WardenError::ServiceNotFound(_)),
        "应为 ServiceNotFound,实际:{err:?}"
    );
}

#[tokio::test]
async fn list_reports_all_registered() {
    let (cmd, args) = common::long_runner();
    let sv = supervisor_with(make_config("a", &cmd, args.clone(), false, 3));
    sv.add(make_config("b", &cmd, args, false, 3)).unwrap();
    let mut names: Vec<_> = sv.list().into_iter().map(|s| s.name).collect();
    names.sort(); // DashMap 无序,排序后比较
    assert_eq!(names, vec!["a", "b"]);
}

#[tokio::test]
async fn logs_captured_from_child_stdout() {
    // ping 会输出多行 stdout,验证 reader task 把它们推入了 LogHub
    let (cmd, args) = common::long_runner();
    let sv = supervisor_with(make_config("loggy", &cmd, args, false, 3));

    sv.start("loggy").await.unwrap();
    wait_for_state(&sv, "loggy", "running", Duration::from_secs(5)).await;
    // 给 reader task 一点时间把输出读入
    tokio::time::sleep(Duration::from_millis(500)).await;

    let log = sv.log_hub("loggy").unwrap();
    let snap = log.snapshot(100);
    assert!(!snap.is_empty(), "应捕获到子进程 stdout 输出");

    sv.stop("loggy").await.unwrap();
}

#[tokio::test]
async fn metrics_sampled_for_running_process() {
    let (cmd, args) = common::long_runner();
    let sv = Arc::new(supervisor_with(make_config("m", &cmd, args, false, 3)));
    sv.start("m").await.unwrap();
    wait_for_state(&sv, "m", "running", Duration::from_secs(5)).await;

    // 启动 metrics 采样(快间隔便于测试),等待至少一次采样
    let sv2 = Arc::clone(&sv);
    sv2.spawn_metrics(Duration::from_millis(300));
    tokio::time::sleep(Duration::from_millis(800)).await;

    let st = sv.status("m").unwrap();
    assert!(st.metrics.sampled_at.is_some(), "应已采样到 metrics");
    assert!(st.metrics.memory_kb > 0, "memory 应为非零");

    sv.stop("m").await.unwrap();
}

/// 意图:reload(apply_config)的唯一数据源语义——消失的服务优雅停止后移除
/// (不制造脱离管理的孤儿句柄)、新服务注册(不启动)。旧实现的 retain 条件
/// 写反(运行中被移出管理表、已停止的被保留),由本测试钉死正确语义。
#[tokio::test]
async fn apply_config_stops_and_removes_disappeared_services() {
    let (cmd, args) = common::long_runner();
    let sv = supervisor_with(make_config("run-a", &cmd, args, false, 3));
    sv.add(make_config("idle-b", "whatever", vec![], false, 3))
        .unwrap();
    sv.start("run-a").await.unwrap();
    wait_for_state(&sv, "run-a", "running", Duration::from_secs(5)).await;

    // 新配置:只剩 new-c(全新)——run-a(运行中)与 idle-b(已停止)都消失
    let new_cfg = Config {
        services: vec![make_config("new-c", "whatever2", vec![], false, 3)],
        ..Default::default()
    };
    sv.apply_config(&new_cfg).await;

    assert!(sv.status("run-a").is_err(), "运行中的 run-a 应被停止并移除");
    assert!(sv.status("idle-b").is_err(), "已停止的 idle-b 应被移除");
    // new-c 注册且未启动(auto_start=false,启动由编排决定)
    assert_eq!(
        sv.status("new-c").unwrap().state.name(),
        "stopped",
        "新服务应注册但不自动启动"
    );
    assert_eq!(sv.names(), vec!["new-c"]);
}

/// 意图:reload 对同名服务做配置原位替换(运行中保留进程,下次启动生效)。
#[tokio::test]
async fn apply_config_updates_existing_service_config() {
    let (cmd, args) = common::long_runner();
    let sv = supervisor_with(make_config("keep", &cmd, args, false, 3));
    let mut changed = make_config("keep", "other-cmd", vec![], false, 3);
    changed.priority = 9;
    sv.apply_config(&Config {
        services: vec![changed],
        ..Default::default()
    })
    .await;
    let st = sv.status("keep").unwrap();
    assert_eq!(st.priority, 9, "reload 应替换同名服务配置");
    assert_eq!(st.state.name(), "stopped");
}
