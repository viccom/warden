//! graceful shutdown e2e:验证 daemon 在 HTTP 长连接(SSE 日志流)未关闭时仍能限期退出。
//!
//! 背景(Windows 实测 bug):浏览器 UI 开着 EventSource 时 Ctrl-C,axum graceful
//! shutdown 无限等待 SSE 连接关闭 → 进程卡住不退出(用户再按 Ctrl-C 被默认 handler
//! 以 0xC000013A 强杀)。修复:serve 设 5s 上限,超时强制断开。
//! 本测试:起 daemon(注册服务但不启动进程)→ 挂一个 SSE 客户端 → cancel →
//! 断言 run_app_with_shutdown 在上限内返回。

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use warden::config::Config;

/// 挑一个空闲端口(OS 分配 :0 后立即释放,写进配置;存在极小竞态,测试可接受)。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn shutdown_returns_within_limit_despite_sse_client() {
    let port = free_port();
    let cfg = Config::parse(&format!(
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
    ))
    .unwrap();

    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let app = tokio::spawn(async move { warden::run_app_with_shutdown(cfg, None, shutdown).await });

    // 等 daemon ready(health 白名单免鉴权)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            // 挂一个 SSE 日志流客户端:发出 GET 后不关连接(模拟浏览器 EventSource)
            let req = format!(
                "GET /api/v1/services/svc/logs/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n"
            );
            use tokio::io::AsyncWriteExt;
            if s.write_all(req.as_bytes()).await.is_ok() {
                // 读到响应头即可,保持连接不 drop
                let mut buf = [0u8; 256];
                use tokio::io::AsyncReadExt;
                let _ = s.read(&mut buf).await;
                std::mem::forget(s); // 连接保持到进程退出,不复关
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon 未在 5s 内就绪"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 先证明没有"启动 N 秒必退"回归:无 shutdown 信号时 daemon 必须存活
    // (曾引入 bug:serve 超时从启动起算,daemon 只活 5s)
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(
        !app.is_finished(),
        "daemon 在无 shutdown 信号时不应退出(疑似 serve 超时起算点回归)"
    );

    cancel.cancel();

    // 断言:serve 5s 上限 + stop_all 余量 → cancel 后 ≤ 10s 内返回
    match tokio::time::timeout(Duration::from_secs(10), app).await {
        Ok(Ok(Ok(()))) => {} // run_app_with_shutdown 正常返回
        Ok(Ok(Err(e))) => panic!("run_app_with_shutdown 返回错误:{e:?}"),
        Ok(Err(e)) => panic!("join 错误:{e:?}"),
        Err(_) => panic!("SSE 连接未关闭时 shutdown 超过 10s 未返回(graceful 卡死回归)"),
    }
}
