#![cfg(feature = "reverse-proxy")]
//! P5 管理面 e2e:路由 CRUD API(写回 services.toml 保注释)+ reload/CRUD 热生效
//! (经 SharedProxyConfig,免重启)+ 路由级 metrics。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tower::ServiceExt; // oneshot
use warden::api::{build_router, build_state, AppState};
use warden::config::Config;
use warden::proxy::{shared_from, HostRouter, ProxyMetrics, ProxyState};
use warden::supervisor::Supervisor;

fn setup(tag: &str, proxy_toml: &str) -> (AppState, String) {
    let d = std::env::temp_dir().join(format!("warden-pxadmin-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let cfg_path = d.join("services.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[daemon]\ndata_dir = \"{}\"\nlog_dir = \"\"\n\n{proxy_toml}\n\n\
             # 既有服务注释:proxy CRUD 不得动我\n[[service]]\nname = \"svc-a\"\ncommand = \"whatever\"\n",
            d.join("data").to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let cfg = Config::load(Some(&cfg_path)).unwrap();
    let state = build_state(cfg, Some(cfg_path.clone()));
    (state, cfg_path.to_string_lossy().into_owned())
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

fn route_json(host: &str, to: &str) -> serde_json::Value {
    serde_json::json!({ "host": host, "to": to })
}

const BASE_PROXY: &str = "[proxy]\ndomain = \"x.example.com\"\nhttp_bind = \"127.0.0.1:0\"\n";

/// 意图:路由 CRUD 全链路——写回配置文件(保注释)+ 引擎热生效(shared)+ 列表。
#[tokio::test]
async fn route_crud_roundtrip_persists_and_lives() {
    let (state, path) = setup("crud", BASE_PROXY);

    // 新增
    let resp = hit(
        &state,
        "POST",
        "/api/v1/proxy/routes",
        Some(route_json("a.x.example.com", "http://127.0.0.1:9000")),
    )
    .await;
    assert_eq!(resp.status(), 200, "新增路由应 200");
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("a.x.example.com"), "写回配置文件:{file}");
    assert!(file.contains("proxy CRUD 不得动我"), "既有注释保留:{file}");

    // 引擎侧即时生效
    let shared = state.proxy_shared.as_ref().expect("[proxy] 存在");
    assert!(
        shared
            .read()
            .unwrap()
            .routes
            .iter()
            .any(|r| r.host == "a.x.example.com"),
        "CRUD 后 shared 即时更新"
    );

    // 列表
    let resp = hit(&state, "GET", "/api/v1/proxy", None).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    let hosts: Vec<&str> = body["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["host"].as_str().unwrap())
        .collect();
    assert!(hosts.contains(&"a.x.example.com"), "列表含新增:{body}");

    // 更新(PUT)
    let resp = hit(
        &state,
        "PUT",
        "/api/v1/proxy/routes/a.x.example.com",
        Some(route_json("a.x.example.com", "http://127.0.0.1:9001")),
    )
    .await;
    assert_eq!(resp.status(), 200, "PUT 更新应 200");
    let cfg = state.proxy_shared.as_ref().unwrap().read().unwrap().clone();
    assert_eq!(
        cfg.routes
            .iter()
            .find(|r| r.host == "a.x.example.com")
            .unwrap()
            .to
            .as_deref(),
        Some("http://127.0.0.1:9001"),
        "PUT 后 shared 更新"
    );

    // 删除
    let resp = hit(
        &state,
        "DELETE",
        "/api/v1/proxy/routes/a.x.example.com",
        None,
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert!(
        !state
            .proxy_shared
            .as_ref()
            .unwrap()
            .read()
            .unwrap()
            .routes
            .iter()
            .any(|r| r.host == "a.x.example.com"),
        "DELETE 后 shared 移除"
    );
    let resp = hit(
        &state,
        "DELETE",
        "/api/v1/proxy/routes/a.x.example.com",
        None,
    )
    .await;
    assert_eq!(resp.status(), 404, "再删 404");
}

/// 意图:重复 host 409;非法路由(to+service 并存/host 空/未知 service)400。
#[tokio::test]
async fn route_crud_rejects_dup_and_invalid() {
    let (state, _path) = setup(
        "reject",
        &format!("{BASE_PROXY}[[proxy.route]]\nhost = \"a.x.example.com\"\nto = \"http://1\"\n"),
    );

    // 重复 host → 409
    let resp = hit(
        &state,
        "POST",
        "/api/v1/proxy/routes",
        Some(route_json("a.x.example.com", "http://2")),
    )
    .await;
    assert_eq!(resp.status(), 409, "重复 host 应 409");

    // to 与 service 并存 → 400
    let resp = hit(
        &state,
        "POST",
        "/api/v1/proxy/routes",
        Some(serde_json::json!({
            "host": "b.x.example.com", "to": "http://1", "service": "svc-a"
        })),
    )
    .await;
    assert_eq!(resp.status(), 400, "to/service 并存应 400");

    // 未知 service 引用 → 400
    let resp = hit(
        &state,
        "POST",
        "/api/v1/proxy/routes",
        Some(serde_json::json!({
            "host": "c.x.example.com", "service": "no-such-svc"
        })),
    )
    .await;
    assert_eq!(resp.status(), 400, "未知 service 应 400");

    // 非法 host → 400
    let resp = hit(
        &state,
        "POST",
        "/api/v1/proxy/routes",
        Some(route_json("bad host", "http://1")),
    )
    .await;
    assert_eq!(resp.status(), 400, "非法 host 应 400");
}

/// 意图:reload 把文件中手工改过的 [proxy] 段同步进引擎(shared)——
/// 显式路由/domain 免重启生效(P5 热生效)。
#[tokio::test]
async fn reload_syncs_proxy_routes_into_engine() {
    let (state, path) = setup(
        "reload",
        &format!("{BASE_PROXY}[[proxy.route]]\nhost = \"a.x.example.com\"\nto = \"http://127.0.0.1:1\"\n"),
    );

    // 手工改文件:换 to
    let file = std::fs::read_to_string(&path).unwrap();
    let edited = file.replace("http://127.0.0.1:1", "http://10.0.0.9:9000");
    std::fs::write(&path, edited).unwrap();

    let resp = hit(&state, "POST", "/api/v1/config/reload", None).await;
    assert_eq!(resp.status(), 200);

    let shared = state.proxy_shared.as_ref().expect("[proxy] 存在");
    let cfg = shared.read().unwrap().clone();
    let r = cfg
        .routes
        .iter()
        .find(|r| r.host == "a.x.example.com")
        .expect("路由仍在");
    assert_eq!(
        r.to.as_deref(),
        Some("http://10.0.0.9:9000"),
        "reload 后热生效"
    );
}

/// 意图:CRUD 写回后引擎侧 shared 同步(新增即生效,不等 reload)。
#[tokio::test]
async fn route_crud_updates_live_engine() {
    let (state, _path) = setup("live", BASE_PROXY);
    let resp = hit(
        &state,
        "POST",
        "/api/v1/proxy/routes",
        Some(route_json("hot.x.example.com", "http://10.1.1.1:80")),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let shared = state.proxy_shared.as_ref().expect("[proxy] 存在");
    let cfg = shared.read().unwrap().clone();
    assert!(
        cfg.routes.iter().any(|r| r.host == "hot.x.example.com"),
        "CRUD 后 shared 即时更新"
    );
}

/// 意图:引擎级 metrics——已路由请求按 host 计数,5xx 计错误;未路由不计数。
#[tokio::test]
async fn engine_metrics_record_routed_requests() {
    let ok_up = spawn_upstream("ok").await;
    let dead = "http://127.0.0.1:1"; // 不可达 → 502

    let cfg = warden::config::ProxyConfig {
        domain: Some("x.example.com".into()),
        http_bind: Some("127.0.0.1:0".into()),
        https_bind: None,
        connect_timeout_ms: 500,
        preserve_host: false,
        cert_file: None,
        key_file: None,
        upstream_ca_file: None,
        acme: Default::default(),
        routes: vec![
            warden::config::ProxyRoute {
                host: "ok.x.example.com".into(),
                to: Some(format!("http://{ok_up}")),
                service: None,
                preserve_host: None,
            },
            warden::config::ProxyRoute {
                host: "dead.x.example.com".into(),
                to: Some(dead.into()),
                service: None,
                preserve_host: None,
            },
        ],
    };
    let metrics = Arc::new(ProxyMetrics::new());
    let state = Arc::new(ProxyState {
        router: HostRouter::new(
            shared_from(cfg.clone()),
            Arc::new(Supervisor::new(PathBuf::from(""))),
        ),
        client: warden::proxy::build_client(&cfg),
        scheme: "http",
        metrics: metrics.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    warden::proxy::spawn_http(
        listener,
        warden::proxy::HttpEntry::Forward(state),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    async fn get_addr(addr: std::net::SocketAddr, host: &str) -> reqwest::StatusCode {
        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .header("host", host)
            .send()
            .await
            .unwrap()
            .status()
    }
    assert_eq!(get_addr(addr, "ok.x.example.com").await, 200);
    assert_eq!(get_addr(addr, "ok.x.example.com").await, 200);
    assert_eq!(get_addr(addr, "dead.x.example.com").await, 502);
    // 未路由(421)不计
    let _ = get_addr(addr, "unknown.x.example.com").await;

    let snap = metrics.snapshot();
    let ok = snap
        .iter()
        .find(|(h, _, _)| h == "ok.x.example.com")
        .unwrap();
    assert_eq!(ok.1, 2, "ok 路由 2 次请求");
    assert_eq!(ok.2, 0, "无 5xx");
    let deadm = snap
        .iter()
        .find(|(h, _, _)| h == "dead.x.example.com")
        .unwrap();
    assert_eq!(deadm.1, 1);
    assert_eq!(deadm.2, 1, "502 计 1 次 5xx");
    assert!(
        !snap.iter().any(|(h, _, _)| h.contains("unknown")),
        "未路由请求不计数"
    );
    shutdown.cancel();
}

async fn spawn_upstream(body: &'static str) -> std::net::SocketAddr {
    let app = Router::new().route("/", get(move || async move { body }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    a
}
