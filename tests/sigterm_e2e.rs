//! SIGTERM e2e:systemd stop / kill 发 SIGTERM,daemon 必须走优雅停机链
//! (停止被监护服务 + drain HTTP 后 exit 0),而非 Unix 默认行为立即终止——
//! 那样子进程只能靠 PDEATHSIG 被 SIGKILL 连带死,graceful 全部旁路。

#![cfg(unix)]

use std::time::Duration;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn sigterm_should_trigger_graceful_shutdown() {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("warden-sigterm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("services.toml");
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
name = "svc"
command = "whatever"
auto_start = false
"#
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_warden"))
        .args(["run", "--config", cfg_path.to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("启动 warden 测试实例");

    // 等 daemon ready(health 免鉴权)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon 未在 5s 内就绪"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 旧缺陷:SIGTERM 默认行为 = 被信号终止(exit status: signal 15);
    // 修复后:优雅停机链完成,exit code 0
    unsafe {
        libc::kill(child.id().unwrap() as i32, libc::SIGTERM);
    }

    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => {
            let status = status.expect("wait 不应出错");
            assert!(
                status.success(),
                "SIGTERM 后应优雅退出 exit 0,实际 {status:?}(被信号终止 = SIGTERM 监听缺失)"
            );
        }
        Err(_) => {
            let _ = child.kill().await;
            panic!("SIGTERM 后 10s 未退出");
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}
