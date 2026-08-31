//! daemon 暴毙清树端到端(Unix):daemon 被 SIGKILL 杀死时,被监护子进程应被内核连带清理,
//! 不残留孤儿 —— 对齐 Windows Job Object `KILL_ON_JOB_CLOSE` 的行为。
//!
//! 实现:拉起真实 warden 二进制(auto_start 一个 sleep 服务),经 HTTP API 拿到子进程 pid,
//! SIGKILL daemon,轮询 /proc 确认子进程消失。
//! 修复前(Linux 无 Job Object 等价物):sleep 被 init 收养继续存活 → 本测试红。

#![cfg(unix)]

use std::time::{Duration, Instant};

use warden::tui::api::ApiClient;

/// cargo 注入的 warden 主程序二进制路径(见 Cargo.toml [[bin]] warden)。
const WARDEN_EXE: &str = env!("CARGO_BIN_EXE_warden");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn proc_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test]
async fn supervised_child_dies_when_daemon_is_sigkilled() {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("warden-pdeath-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("pdeath.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"
[daemon]
api_bind = "127.0.0.1:{port}"
auth_token = ""
data_dir = ""
log_dir = ""

[[service]]
name = "sleeper"
command = "sleep"
args = ["60"]
auto_start = true
"#
        ),
    )
    .unwrap();

    let mut daemon = std::process::Command::new(WARDEN_EXE)
        .args(["run", "--config", cfg_path.to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("拉起 warden daemon");

    // 等 daemon 就绪且 sleeper 拿到 pid
    let client = ApiClient::new(format!("http://127.0.0.1:{port}"), None).unwrap();
    let child_pid = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(list) = client.list().await {
                if let Some(pid) = list
                    .iter()
                    .find(|s| s.name == "sleeper")
                    .and_then(|s| s.pid())
                {
                    break pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "sleeper 未进入 running(未拿到 pid)"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    assert!(
        proc_alive(child_pid).await,
        "测试前提:SIGKILL 前子进程应存活"
    );

    // 暴毙 daemon(不给任何优雅机会,模拟崩溃/OOM kill -9)
    unsafe {
        libc::kill(daemon.id() as i32, libc::SIGKILL);
    }
    let _ = daemon.wait(); // 回收僵尸

    // 断言:子进程应随 daemon 死亡被清理(PDEATHSIG),而非被 init 收养存活
    let deadline = Instant::now() + Duration::from_secs(3);
    while proc_alive(child_pid).await {
        assert!(
            Instant::now() < deadline,
            "daemon 被 SIGKILL 后子进程(pid={child_pid})仍存活——孤儿清理未生效"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = std::fs::remove_dir_all(&dir);
}
