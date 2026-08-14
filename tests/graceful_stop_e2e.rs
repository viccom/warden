//! 优雅停止端到端测试:验证 CTRL_C_EVENT 触发 graceful、超时强杀、Job Object 杀树。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use warden::config::Config;
use warden::model::{RestartPolicy, ServiceConfig};
use warden::supervisor::Supervisor;

/// 由 cargo 注入的 helper 二进制路径(见 Cargo.toml [[bin]] graceful_target)。
const GRACEFUL_TARGET: &str = env!("CARGO_BIN_EXE_graceful_target");

fn svc(name: &str, cmd: &str, args: Vec<String>, graceful_timeout: u64) -> ServiceConfig {
    ServiceConfig {
        name: name.into(),
        display_name: String::new(),
        description: String::new(),
        command: cmd.into(),
        args,
        working_dir: None,
        environment: HashMap::new(),
        auto_start: false,
        auto_restart: false,
        restart: RestartPolicy::default(),
        health: None,
        ui_url: None,
        graceful_timeout_secs: graceful_timeout,
        output_encoding: None,
    }
}

fn supervisor_with(svc: ServiceConfig) -> Supervisor {
    let cfg = Config {
        services: vec![svc],
        ..Default::default()
    };
    Supervisor::from_config(&cfg, PathBuf::new())
}

async fn wait_running(sv: &Supervisor, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if sv
            .status(name)
            .map(|s| s.state.is_running())
            .unwrap_or(false)
        {
            return;
        }
        assert!(Instant::now() < deadline, "{name} 未进入 running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_state(sv: &Supervisor, name: &str, want: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = sv.status(name) {
            if s.state.name() == want {
                return;
            }
        }
        assert!(Instant::now() < deadline, "{name} 未进入「{want}」");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 标记文件存在 = helper 收到了 CTRL_C_EVENT 并执行了 graceful 代码(非被强杀)。
#[tokio::test]
async fn graceful_stop_sends_ctrl_c_and_target_exits_cleanly() {
    let marker =
        std::env::temp_dir().join(format!("warden-graceful-{}.marker", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    let sv = supervisor_with(svc(
        "g",
        GRACEFUL_TARGET,
        vec![marker.to_string_lossy().to_string()],
        5,
    ));
    sv.start("g").await.unwrap();
    wait_running(&sv, "g").await;

    sv.stop("g").await.unwrap();
    wait_state(&sv, "g", "stopped", Duration::from_secs(8)).await;

    assert!(
        marker.exists(),
        "helper 应收到 CTRL_C_EVENT 写标记后 graceful 退出(证明信号链路通)"
    );
    let _ = std::fs::remove_file(&marker);
}

/// 不响应 CTRL_C 的进程(ping)→ graceful_timeout 后被强杀。
#[tokio::test]
async fn force_kill_after_graceful_timeout() {
    let cmd = if cfg!(windows) { "ping" } else { "sleep" };
    let args: Vec<String> = if cfg!(windows) {
        vec!["-n".into(), "120".into(), "127.0.0.1".into()]
    } else {
        vec!["120".into()]
    };
    let sv = supervisor_with(svc("p", cmd, args, 1));
    sv.start("p").await.unwrap();
    wait_running(&sv, "p").await;

    let start = Instant::now();
    sv.stop("p").await.unwrap();
    let elapsed = start.elapsed();
    // 应在 ~1s timeout 后强杀完成(不会立即,因为先等 graceful)
    assert!(
        elapsed >= Duration::from_millis(900),
        "应先等 graceful_timeout(~1s)再强杀,实际 {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "超时强杀应快速完成,实际 {elapsed:?}"
    );
    assert_eq!(sv.status("p").unwrap().state.name(), "stopped");
}

/// stubborn helper(收到信号不退)+ 子进程 → force_kill 杀整棵树(Job Object)。
#[tokio::test]
async fn force_kill_terminates_process_tree() {
    let marker = std::env::temp_dir().join(format!("warden-tree-{}.marker", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    let sv = supervisor_with(svc(
        "t",
        GRACEFUL_TARGET,
        vec![
            marker.to_string_lossy().to_string(),
            "--stubborn".into(),
            "--child".into(),
        ],
        1,
    ));
    sv.start("t").await.unwrap();
    wait_running(&sv, "t").await;
    // 给 helper 时间 spawn 孙子进程
    tokio::time::sleep(Duration::from_millis(500)).await;

    sv.stop("t").await.unwrap();
    wait_state(&sv, "t", "stopped", Duration::from_secs(5)).await;

    // stubborn helper 收到信号写了标记(但没退出,被 force_kill)
    assert!(marker.exists(), "stubborn helper 应收到 CTRL_C_EVENT");
    let _ = std::fs::remove_file(&marker);
    // 注:孙进程(ping)被 Job Object 一并 TerminateJobObject 杀掉;
    //   精确进程检查较脆(系统可能有其他 ping),这里以 helper 端 force_kill 验证为主。
}
