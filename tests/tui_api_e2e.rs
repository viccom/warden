//! TUI ApiClient 契约 e2e:起真实 daemon(run_app_with_shutdown),用 ApiClient
//! 打真实 HTTP——验证 TUI 客户端与后端 API 契约一致(防前后端脱节,如字段改名)。

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use warden::config::Config;
use warden::tui::api::ApiClient;

mod common;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn api_client_matches_backend_contract() {
    let port = free_port();
    let (cmd, args) = common::long_runner(); // 跨平台无害命令(ping/sleep)
    let args_str = serde_json::to_string(&args).unwrap();
    let cfg = Config::parse(&format!(
        r#"
[daemon]
api_bind = "127.0.0.1:{port}"
auth_token = ""
data_dir = ""
log_dir = ""

[[service]]
name = "demo"
command = "{cmd}"
args = {args_str}
auto_start = false
graceful_timeout_secs = 1
"#
    ))
    .unwrap();

    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let daemon =
        tokio::spawn(async move { warden::run_app_with_shutdown(cfg, None, shutdown).await });

    // 等 daemon 就绪(health 白名单)
    let client = ApiClient::new(format!("http://127.0.0.1:{port}"), None).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if client.health().await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "daemon 未就绪");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 1. list:契约(字段名/state 形态/display_name 回退)
    let list = client.list().await.expect("list 应成功");
    assert_eq!(list.len(), 1);
    let svc = &list[0];
    assert_eq!(svc.name, "demo");
    assert_eq!(svc.label(), "demo"); // display_name 空回退 name
    assert_eq!(svc.state_name(), "stopped"); // ProcState tag 形态解析

    // 2. start → 状态 running;stop → stopped(生命周期契约)
    client.action("demo", "start").await.expect("start 应成功");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let s = &client.list().await.unwrap()[0];
        if s.state_name() == "running" {
            assert!(s.pid().is_some(), "running 应有 pid");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "未进入 running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client.action("demo", "stop").await.expect("stop 应成功");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if client.list().await.unwrap()[0].state_name() == "stopped" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "未回到 stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 3. start_all / stop_all 端点(连字符路由)
    client.start_all().await.expect("start_all 应成功");
    client.stop_all().await.expect("stop_all 应成功");

    // 4. logs_tail:契约(lines[].stream/ts/text)
    let lines = client
        .logs_tail("demo", 10)
        .await
        .expect("logs_tail 应成功");
    // 至少应有 warden 自身打的状态行(停止后 Stopped 行)
    assert!(!lines.is_empty(), "demo 应至少有一条日志");

    // 5. SSE 流:能连上并收到 event(log)(起服务让其输出)
    client.action("demo", "start").await.unwrap();
    let es = client.logs_stream("demo");
    let mut es = es;
    use futures_util::StreamExt;
    let got = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(item) = es.next().await {
            if let Ok(reqwest_eventsource::Event::Message(m)) = item {
                if m.event == "log" {
                    return true;
                }
            }
        }
        false
    })
    .await;
    assert!(got.unwrap_or(false), "SSE 应收到 log 事件");

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), daemon)
        .await
        .expect("daemon 应限期退出");
}
