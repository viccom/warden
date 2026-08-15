//! API 集成测试:用 tower oneshot 直接打 build_router(不发真实网络请求)。

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use warden::api::{build_router, build_state};
use warden::config::Config;
use warden::model::{RestartPolicy, ServiceConfig};
use warden::supervisor::Supervisor;

fn service(name: &str, cmd: &str, args: Vec<String>) -> ServiceConfig {
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
        graceful_timeout_secs: 1,
        output_encoding: None,
        group: None,
        priority: 0,
    }
}

fn app_with(services: Vec<ServiceConfig>, token: Option<&str>) -> (axum::Router, Arc<Supervisor>) {
    let mut cfg = Config {
        services,
        daemon: Default::default(),
    };
    if let Some(t) = token {
        cfg.daemon.auth_token = t.into();
    }
    let state = build_state(cfg, None);
    let sv = state.supervisor.clone();
    (build_router(state), sv)
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
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
        if Instant::now() >= deadline {
            panic!("{name} 未进入 running");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn health_ok_without_auth() {
    let (app, _sv) = app_with(vec![], None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("\"status\":\"ok\""));
}

#[tokio::test]
async fn list_returns_registered_services() {
    let (cmd, args) = common::long_runner();
    let (app, _sv) = app_with(vec![service("alpha", &cmd, args)], None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("alpha"));
}

#[tokio::test]
async fn start_then_stop_via_api() {
    let (cmd, args) = common::long_runner();
    let (app, sv) = app_with(vec![service("beta", &cmd, args)], None);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/services/beta/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_running(&sv, "beta").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/services/beta/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(sv.status("beta").unwrap().state.name(), "stopped");
}

#[tokio::test]
async fn unknown_service_returns_404() {
    let (app, _sv) = app_with(vec![], None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/services/nope/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn logs_snapshot_after_start() {
    let (cmd, args) = common::long_runner();
    let (app, sv) = app_with(vec![service("loggy", &cmd, args)], None);

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/services/loggy/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    wait_running(&sv, "loggy").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/services/loggy/logs?tail=50")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("lines"));
    let _ = sv.stop("loggy").await;
}

#[tokio::test]
async fn auth_rejects_without_token_when_configured() {
    let (cmd, args) = common::long_runner();
    let (app, _sv) = app_with(vec![service("x", &cmd, args)], Some("secret"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_accepts_with_correct_token_and_health_is_public() {
    let (cmd, args) = common::long_runner();
    let (app, _sv) = app_with(vec![service("x", &cmd, args)], Some("secret"));

    // health 即便配置了 token 也放行
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 带 token 访问受保护端点
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/services")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 意图:组级启停端点按组作用(组内优先级序),desired-state 随组操作同步。
#[tokio::test]
async fn group_routes_start_and_stop_scoped() {
    let (cmd, args) = common::long_runner();
    let web = |name: &str| ServiceConfig {
        group: Some("web".into()),
        ..service(name, &cmd, args.clone())
    };
    let (app, sv) = app_with(
        vec![web("w1"), web("w2"), service("other", &cmd, args)],
        None,
    );

    // 组启动
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/groups/web/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "组启动应 200");
    wait_running(&sv, "w1").await;
    wait_running(&sv, "w2").await;
    assert!(
        !sv.status("other").unwrap().state.is_running(),
        "组外服务不应被拉起"
    );

    // 组停止
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/groups/web/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "组停止应 200");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let both_stopped = ["w1", "w2"]
            .iter()
            .all(|n| sv.status(n).map(|s| !s.state.is_running()).unwrap_or(false));
        if both_stopped {
            break;
        }
        assert!(Instant::now() < deadline, "组服务未全部停止");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 意图:桌面版(Tauri webview,origin http://tauri.localhost)跨域访问 API 不能被 CORS 拦截;
/// 仅放行 Tauri 相关 origin(桌面 webview/dev server),不开放任意来源。
#[tokio::test]
async fn cors_allows_tauri_webview_origin_only() {
    let (app, _sv) = app_with(vec![], None);

    // 预检(preflight):tauri.localhost 应被放行并回显 origin
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/v1/services")
                .header("origin", "http://tauri.localhost")
                .header("access-control-request-method", "GET")
                .header("access-control-request-headers", "authorization")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "预检应 200");
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "http://tauri.localhost"
    );

    // 实际 GET 带 origin:响应回显 allow-origin(浏览器侧才放行读取)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("origin", "http://tauri.localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "http://tauri.localhost"
    );

    // 未知 origin:不回显(浏览器仍拦),且请求本体不受影响(非浏览器客户端无 origin 语义)
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("origin", "http://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "未知 origin 请求本体仍正常处理"
    );
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "未知 origin 不应回显 allow-origin"
    );
}
