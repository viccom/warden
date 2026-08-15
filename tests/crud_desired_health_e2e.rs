//! Phase 4 e2e:运行时 CRUD + desired-state 持久化 + 健康检查。
//!
//! 1. CRUD:POST/PUT/DELETE /api/v1/services 全路径(含运行中拒绝改/删)。
//! 2. overlay:CRUD 服务落盘 runtime_services.toml,重建 state(模拟 daemon 重启)后仍在。
//! 3. desired:API start → desired_state.json=true;重建 state 后 start_desired 恢复启动。
//! 4. health:真 TcpListener 起本地端口,服务指向它 → healthy;关闭 → unhealthy(迁移)。

mod common;

use std::time::Duration;

use tower::ServiceExt; // oneshot
use warden::api::{build_router, build_state, AppState};
use warden::config::Config;

fn base_cfg(port: u16, data_dir: &str) -> Config {
    Config::parse(&format!(
        r#"
[daemon]
api_bind = "127.0.0.1:{port}"
auth_token = ""
data_dir = "{data_dir}"
log_dir = ""

[[service]]
name = "file-svc"
command = "whatever"
auto_start = false
"#
    ))
    .unwrap()
}

async fn hit(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Response<axum::body::Body> {
    use axum::body::Body;
    use axum::http::Request;
    let b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    build_router(state.clone()).oneshot(req).await.unwrap()
}

fn tmpdir(tag: &str) -> String {
    let d = std::env::temp_dir().join(format!("warden-p4-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.to_string_lossy().replace('\\', "/")
}

/// CRUD 全路径 + overlay 持久化(重建 state 后 CRUD 服务仍在)。
#[tokio::test]
async fn crud_create_update_delete_and_overlay_persist() {
    let data_dir = tmpdir("crud");
    let state = build_state(base_cfg(0, &data_dir), None);

    // create:合法 body
    let (cmd, args) = common::long_runner();
    let body = serde_json::json!({
        "name": "runtime-svc",
        "command": cmd,
        "args": args,
        "auto_start": false,
        "graceful_timeout_secs": 1,
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);
    // 落盘存在
    assert!(std::path::Path::new(&data_dir)
        .join("runtime_services.toml")
        .exists());

    // create:重名拒绝(409/400 家族,非 200)
    let dup = serde_json::json!({ "name": "runtime-svc", "command": "x" });
    let r = hit(&state, "POST", "/api/v1/services", Some(dup)).await;
    assert_ne!(r.status(), 200, "重名应拒绝");

    // config 端点:返回完整配置(编辑表单预填用)
    let r = hit(&state, "GET", "/api/v1/services/runtime-svc/config", None).await;
    assert_eq!(r.status(), 200, "config 端点应 200");
    {
        use axum::body::to_bytes;
        let bytes = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let cfg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cfg["name"], "runtime-svc");
        assert_eq!(cfg["command"], cmd, "config 应含完整 command");
    }

    // create:坏 name(禁用字符)拒绝
    let bad = serde_json::json!({ "name": "a/b", "command": "x" });
    let r = hit(&state, "POST", "/api/v1/services", Some(bad)).await;
    assert_ne!(r.status(), 200, "禁用字符 name 应拒绝");

    // update:名称不一致拒绝
    let up = serde_json::json!({ "name": "mismatch", "command": "y" });
    let r = hit(&state, "PUT", "/api/v1/services/runtime-svc", Some(up)).await;
    assert_ne!(r.status(), 200, "body.name 与路径不一致应拒绝");

    // update:一致 → 200
    let up = serde_json::json!({ "name": "runtime-svc", "command": cmd, "args": args });
    let r = hit(&state, "PUT", "/api/v1/services/runtime-svc", Some(up)).await;
    assert_eq!(r.status(), 200);

    // delete:200
    let r = hit(&state, "DELETE", "/api/v1/services/runtime-svc", None).await;
    assert_eq!(r.status(), 200);

    // 再造一个,验证重建 state(daemon 重启模拟)后 overlay 恢复
    let body = serde_json::json!({ "name": "persist-svc", "command": cmd, "args": args });
    hit(&state, "POST", "/api/v1/services", Some(body)).await;
    let state2 = build_state(base_cfg(0, &data_dir), None);
    let names = state2.supervisor.names();
    assert!(
        names.contains(&"persist-svc".to_string()),
        "重建后 overlay 服务应恢复:{names:?}"
    );
    assert!(
        names.contains(&"file-svc".to_string()),
        "主配置服务仍在:{names:?}"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 运行中的服务:PUT/DELETE 应拒绝(409)。
#[tokio::test]
async fn crud_rejects_while_running() {
    let data_dir = tmpdir("run");
    let state = build_state(base_cfg(0, &data_dir), None);
    // POST 造一个可运行服务并启动
    let (cmd, args) = common::long_runner();
    let body = serde_json::json!({
        "name": "busy",
        "command": cmd,
        "args": args,
        "graceful_timeout_secs": 1,
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);
    state.supervisor.start("busy").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if state
            .supervisor
            .list()
            .iter()
            .any(|s| s.name == "busy" && s.state.is_running())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "busy 未进入 running"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 运行中:PUT/DELETE 应 409
    let up = serde_json::json!({ "name": "busy", "command": "x" });
    let r = hit(&state, "PUT", "/api/v1/services/busy", Some(up)).await;
    assert_eq!(r.status(), 409, "运行中 PUT 应 409");
    let r = hit(&state, "DELETE", "/api/v1/services/busy", None).await;
    assert_eq!(r.status(), 409, "运行中 DELETE 应 409");
    state.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// group/priority 经 CRUD 写入 → 状态快照透出 → overlay 重建后不丢。
#[tokio::test]
async fn crud_group_priority_roundtrip() {
    let data_dir = tmpdir("groupprio");
    let state = build_state(base_cfg(0, &data_dir), None);
    let (cmd, args) = common::long_runner();

    let body = serde_json::json!({
        "name": "grouped-svc",
        "command": cmd,
        "args": args,
        "group": "edge",
        "priority": 7,
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);

    // 状态快照透出 group/priority(TUI/Web 列表用)
    let r = hit(&state, "GET", "/api/v1/services/grouped-svc", None).await;
    assert_eq!(r.status(), 200);
    {
        use axum::body::to_bytes;
        let bytes = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let s: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(s["group"], "edge", "状态快照应含 group:{s}");
        assert_eq!(s["priority"], 7, "状态快照应含 priority:{s}");
    }

    // 重建 state(daemon 重启模拟)后 overlay 恢复,group/priority 不丢
    let state2 = build_state(base_cfg(0, &data_dir), None);
    let r = hit(&state2, "GET", "/api/v1/services/grouped-svc/config", None).await;
    assert_eq!(r.status(), 200);
    {
        use axum::body::to_bytes;
        let bytes = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let cfg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cfg["group"], "edge", "重建后 config 应保留 group:{cfg}");
        assert_eq!(cfg["priority"], 7, "重建后 config 应保留 priority:{cfg}");
    }

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// desired-state:API start 写 true;重建 state(模拟重启)后 start_desired 恢复。
#[tokio::test]
async fn desired_state_persists_and_restores() {
    let data_dir = tmpdir("desired");
    let state = build_state(base_cfg(0, &data_dir), None);
    // 注意 file-svc command=whatever 无法真正启动——desired 标记在 start 失败时也不该写。
    // 这里直接验证:set_desired + start_desired 的 Supervisor 层语义。
    state.supervisor.set_desired("file-svc", true);
    let raw = std::fs::read_to_string(std::path::Path::new(&data_dir).join("desired_state.json"))
        .unwrap();
    assert!(raw.contains("\"file-svc\": true"), "desired 落盘内容:{raw}");

    // 重建:start_auto(auto_start=false 不启动)→ start_desired 尝试启动 file-svc
    // (command 无效会 Failed,但证明"恢复期望"路径被执行——用 status 落在 failed/stopped 佐证)
    let state2 = build_state(base_cfg(0, &data_dir), None);
    state2.supervisor.start_desired().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let st = state2.supervisor.list()[0].state.name().to_string();
    assert!(
        st == "failed" || st == "running",
        "start_desired 应尝试启动(无效 command → failed):实际 {st}"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 健康检查:真 TcpListener healthy → 关闭 unhealthy(状态迁移 + LogHub 告警行)。
#[tokio::test]
async fn health_check_transitions_and_alerts() {
    let data_dir = tmpdir("health");
    // 本地真端口
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hport = listener.local_addr().unwrap().port();
    let cfg = Config::parse(&format!(
        r#"
[daemon]
data_dir = "{data_dir}"
log_dir = ""

[[service]]
name = "h"
command = "whatever"
auto_start = false
health = {{ type = "tcp", host = "127.0.0.1", port = {hport}, timeout_ms = 500, interval_secs = 1 }}
"#
    ))
    .unwrap();
    let state = build_state(cfg, None);
    let _h = warden::supervisor::health::spawn_health(state.supervisor.clone(), None);

    // 手动把状态置 Running(不真起进程——health task 只看状态与端口)
    {
        let names = state.supervisor.names();
        let h = state.supervisor.log_hub(&names[0]).unwrap();
        let _ = h; // 拿 hub 供后面查告警
    }
    let handle = {
        // 直接修改内部状态:通过 start(会失败)不行——用 metrics 采样同款内部遍历?
        // 简化:模拟 Running 需要走真实路径。改为:直接构造 Supervisor 不够优雅。
        // 最实用:创建一个真的 running 服务(ping)+ health 指向本地端口。
        state.supervisor.remove("h").unwrap();
        state.supervisor.add(warden::model::ServiceConfig {
            name: "h2".into(),
            display_name: String::new(),
            description: String::new(),
            command: if cfg!(windows) {
                "ping".into()
            } else {
                "sleep".into()
            },
            args: if cfg!(windows) {
                vec!["-n".into(), "30".into(), "127.0.0.1".into()]
            } else {
                vec!["30".into()]
            },
            working_dir: None,
            environment: Default::default(),
            auto_start: false,
            auto_restart: false,
            restart: Default::default(),
            health: Some(warden::model::HealthCheck::Tcp {
                host: "127.0.0.1".into(),
                port: hport,
                timeout_ms: 500,
                interval_secs: 1,
            }),
            ui_url: None,
            graceful_timeout_secs: 2,
            output_encoding: None,
            group: None,
            priority: 0,
        });
        state.supervisor.start("h2").await.unwrap();
        state.supervisor.log_hub("h2").unwrap()
    };
    // 等 health task 首查(1s tick + interval 1s)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let healthy = state
            .supervisor
            .list()
            .iter()
            .any(|s| s.health.status == "healthy");
        if healthy {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "未进入 healthy(端口开着)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // 关端口 → unhealthy + LogHub 告警行
    drop(listener);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let unhealthy = state
            .supervisor
            .list()
            .iter()
            .any(|s| s.health.status == "unhealthy");
        if unhealthy {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "未迁移 unhealthy(端口已关)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let alerts: Vec<String> = handle
        .snapshot(50)
        .iter()
        .filter(|l| l.text.contains("健康状态迁移"))
        .map(|l| l.text.clone())
        .collect();
    assert!(!alerts.is_empty(), "LogHub 应有健康迁移告警行");
    assert!(
        alerts.iter().any(|a| a.contains("unhealthy")),
        "含 unhealthy 迁移:{alerts:?}"
    );

    state.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(&data_dir);
}
