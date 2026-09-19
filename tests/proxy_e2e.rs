#![cfg(feature = "reverse-proxy")]
//! 反向代理 e2e。本地上游用 tokio::spawn + axum 随机端口,不固定端口。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use warden::config::{ProxyConfig, ProxyRoute};
use warden::model::ServiceConfig;
use warden::supervisor::Supervisor;

const DOMAIN: &str = "opc.dongx.site";

// ── 测试基建 ────────────────────────────────────────────────────

/// 起一个本地上游(随机端口),GET / 返回固定 body。
async fn plain_upstream() -> SocketAddr {
    let app = Router::new().route("/", get(|| async { "upstream-ok" }));
    spawn_upstream(app).await
}

async fn spawn_upstream(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// 起代理:给定路由表与已注册服务(auto 路由来源),返回代理地址。
async fn spawn_proxy_with(routes: Vec<ProxyRoute>, services: Vec<ServiceConfig>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = ProxyConfig {
        domain: Some(DOMAIN.into()),
        http_bind: Some("127.0.0.1:0".into()),
        https_bind: None,
        connect_timeout_ms: 2000,
        preserve_host: false,
        routes,
    };
    let supervisor = Arc::new(Supervisor::new(std::path::PathBuf::from("")));
    for svc in services {
        supervisor.add(svc).unwrap();
    }
    let shutdown = CancellationToken::new();
    warden::proxy::spawn(cfg, supervisor, listener, shutdown);
    addr
}

async fn spawn_proxy(routes: Vec<ProxyRoute>) -> SocketAddr {
    spawn_proxy_with(routes, vec![]).await
}

fn route(host: &str, to: String) -> ProxyRoute {
    ProxyRoute {
        host: host.into(),
        to: Some(to),
        service: None,
        preserve_host: None,
    }
}

/// proxy=true 的服务(不启动,状态 Stopped;ui_url 指向给定上游)。
fn auto_service(name: &str, subdomain: Option<&str>, ui: String) -> ServiceConfig {
    ServiceConfig {
        name: name.into(),
        command: "/bin/true".into(),
        proxy: true,
        subdomain: subdomain.map(Into::into),
        ui_url: Some(ui),
        ..Default::default()
    }
}

async fn get_via_proxy(proxy: SocketAddr, host: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://{proxy}/"))
        .header("host", host)
        .send()
        .await
        .unwrap()
}

// ── Task 9 最简用例 ────────────────────────────────────────────

#[tokio::test]
async fn proxies_get_body() {
    let up = plain_upstream().await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;

    let resp = get_via_proxy(proxy_addr, "fs.opc.dongx.site").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "upstream-ok");
}

// ── Task 10 补全用例 ───────────────────────────────────────────

/// 意图:SSE 不被缓冲——事件按上游节奏逐条到达(弱断言:到达时间跨度
/// ≥ 100ms;缓冲实现会在流结束时一次性到达,跨度≈0)。
#[tokio::test]
async fn streams_sse_without_buffering() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::stream;

    let app = Router::new().route(
        "/events",
        get(|| async {
            let body = stream::unfold(0u8, |i| async move {
                if i >= 3 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                Some((
                    Ok::<_, std::convert::Infallible>(Event::default().data(format!("ev-{i}"))),
                    i + 1,
                ))
            });
            Sse::new(body).keep_alive(KeepAlive::default())
        }),
    );
    let up = spawn_upstream(app).await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;

    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        reqwest::Client::new()
            .get(format!("http://{proxy_addr}/events"))
            .header("host", "fs.opc.dongx.site")
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status(), 200);

    let mut stream = resp.bytes_stream();
    let mut body = String::new();
    let mut arrivals: Vec<Instant> = vec![];
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
    {
        let chunk = chunk.unwrap();
        if !chunk.is_empty() {
            arrivals.push(Instant::now());
            body.push_str(&String::from_utf8_lossy(&chunk));
        }
        if body.contains("ev-2") {
            break;
        }
    }
    // 内容按序完整
    for ev in ["ev-0", "ev-1", "ev-2"] {
        assert!(body.contains(ev), "SSE 应含 {ev},实际:{body}");
    }
    let i0 = body.find("ev-0").unwrap();
    let i1 = body.find("ev-1").unwrap();
    let i2 = body.find("ev-2").unwrap();
    assert!(i0 < i1 && i1 < i2, "事件按序到达");
    // 非一次性到达:首末 chunk 跨度 ≥ 100ms(上游节奏 150ms/条;若被缓冲,
    // 三条在流尾同时到达,跨度接近 0)
    let span = arrivals
        .last()
        .unwrap()
        .duration_since(*arrivals.first().unwrap());
    assert!(
        span >= Duration::from_millis(100),
        "SSE 被缓冲了(跨度 {span:?},应 ≥ 100ms)"
    );
}

/// 意图:大请求体(8 MiB)流式透传——上游收满且 sha256 一致(零缓冲/零截断)。
#[tokio::test]
async fn streams_large_request_body() {
    use axum::http::HeaderMap;
    use sha2::{Digest, Sha256};

    // 测试上游放开 body limit(默认 2 MiB 会提前 413+断连,测不到透传)
    let app = Router::new()
        .route(
            "/hash",
            axum::routing::post(|_headers: HeaderMap, body: axum::body::Bytes| async move {
                let mut h = Sha256::new();
                h.update(&body);
                let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
                format!("{} {}", body.len(), hex)
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024));
    let up = spawn_upstream(app).await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;

    let payload: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut h = Sha256::new();
    h.update(&payload);
    let expect: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/hash"))
        .header("host", "fs.opc.dongx.site")
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.text().await.unwrap(),
        format!("8388608 {expect}"),
        "上游应收满 8 MiB 且哈希一致"
    );
}

/// 意图:未知 Host(无路由且 auto 无此服务)→ 421,防被当开放代理。
#[tokio::test]
async fn returns_421_for_unknown_host() {
    let up = plain_upstream().await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;
    let resp = get_via_proxy(proxy_addr, "b.opc.dongx.site").await;
    assert_eq!(resp.status(), 421);
    let body = resp.text().await.unwrap();
    assert!(body.contains("b.opc.dongx.site"), "错误页含 Host:{body}");
}

/// 意图:恶意 Host 注入的标记必须被 HTML 转义——错误页会被浏览器渲染,
/// 原样嵌入即反射型 XSS(对齐 Web UI textContent 防线)。
#[tokio::test]
async fn error_page_escapes_malicious_host() {
    let up = plain_upstream().await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;
    let resp = get_via_proxy(proxy_addr, "<script>alert(1)</script>.opc.dongx.site").await;
    assert_eq!(resp.status(), 421);
    let body = resp.text().await.unwrap();
    assert!(!body.contains("<script"), "脚本标记必须转义:{body}");
    assert!(body.contains("&lt;script&gt;"), "应实体化呈现:{body}");
}

/// 意图:上游连接拒绝(无人监听端口)→ 502 错误页。
#[tokio::test]
async fn returns_502_when_upstream_refused() {
    // 绑一个端口拿到地址后立刻放弃 → 该端口大概率无人监听
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = probe.local_addr().unwrap();
    drop(probe);

    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{dead}"))]).await;
    let resp = get_via_proxy(proxy_addr, "fs.opc.dongx.site").await;
    assert_eq!(resp.status(), 502);
}

/// 意图:auto 路由引用的服务未运行 → 503,页面含状态文案
/// (supervisor 反代的差异化能力:明确告知服务状态而非裸 502)。
#[tokio::test]
async fn auto_service_stopped_returns_503_with_state() {
    let up = plain_upstream().await;
    let svc = auto_service("fs", None, format!("http://{up}"));
    let proxy_addr = spawn_proxy_with(vec![], vec![svc]).await;

    let resp = get_via_proxy(proxy_addr, "fs.opc.dongx.site").await;
    assert_eq!(resp.status(), 503);
    let body = resp.text().await.unwrap();
    assert!(body.contains("stopped"), "503 页应含服务状态文案:{body}");
    assert!(body.contains("fs"), "503 页应含服务名:{body}");
}

/// 意图:subdomain 覆盖 name——fs2 标签命中(503,服务未运行),
/// fs 不再是标签(421)。
#[tokio::test]
async fn auto_uses_subdomain_override() {
    let up = plain_upstream().await;
    let svc = auto_service("fs", Some("fs2"), format!("http://{up}"));
    let proxy_addr = spawn_proxy_with(vec![], vec![svc]).await;

    let resp = get_via_proxy(proxy_addr, "fs2.opc.dongx.site").await;
    assert_eq!(resp.status(), 503, "fs2 标签应命中 auto 路由(服务停止→503)");
    let resp = get_via_proxy(proxy_addr, "fs.opc.dongx.site").await;
    assert_eq!(resp.status(), 421, "subdomain 覆盖后 name 不再是标签");
}

/// 意图:同 host 显式路由胜出 auto——返回显式上游 body 而非 auto 的 503。
#[tokio::test]
async fn explicit_route_overrides_auto() {
    let svc = auto_service("fs", Some("fs2"), "http://127.0.0.1:1".into());
    let other = spawn_upstream(Router::new().route("/", get(|| async { "explicit-wins" }))).await;
    let proxy_addr = spawn_proxy_with(
        vec![route("fs2.opc.dongx.site", format!("http://{other}"))],
        vec![svc],
    )
    .await;

    let resp = get_via_proxy(proxy_addr, "fs2.opc.dongx.site").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "explicit-wins");
}

/// 意图:通配路由只匹配单层子域(anything.opc.dongx.site 命中,a.b.* 不中)。
#[tokio::test]
async fn wildcard_matches_single_label() {
    let up = plain_upstream().await;
    let proxy_addr = spawn_proxy(vec![route(&format!("*.{DOMAIN}"), format!("http://{up}"))]).await;

    let resp = get_via_proxy(proxy_addr, "anything.opc.dongx.site").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "upstream-ok");

    let resp = get_via_proxy(proxy_addr, "a.b.opc.dongx.site").await;
    assert_eq!(resp.status(), 421, "通配不跨多层");
}

/// 意图:上游收到重写后的头——X-Forwarded-* 正确、Host 已重写为上游、
/// hop-by-hop(Connection 及其令牌)已剥。
#[tokio::test]
async fn upstream_receives_forwarded_headers() {
    use axum::Json;
    use serde_json::json;

    let app = Router::new().route(
        "/",
        get(|headers: axum::http::HeaderMap| async move {
            let pick = |k: &str| {
                headers
                    .get(k)
                    .map(|v| v.to_str().unwrap_or_default().to_owned())
            };
            Json(json!({
                "host": pick("host"),
                "x-forwarded-for": pick("x-forwarded-for"),
                "x-forwarded-proto": pick("x-forwarded-proto"),
                "x-forwarded-host": pick("x-forwarded-host"),
                "connection": pick("connection"),
                "x-custom": pick("x-custom"),
            }))
        }),
    );
    let up = spawn_upstream(app).await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{proxy_addr}/"))
        .header("host", "fs.opc.dongx.site")
        // x-custom 列入 Connection 令牌 → 按 RFC 7230 须随 Connection 一起剥
        .header("connection", "keep-alive, x-custom")
        .header("x-custom", "leak-me")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["host"], format!("{up}"), "Host 应重写为上游 authority");
    assert_eq!(v["x-forwarded-for"], "127.0.0.1", "XFF 应含客户端 IP");
    assert_eq!(v["x-forwarded-proto"], "http");
    assert_eq!(
        v["x-forwarded-host"], "fs.opc.dongx.site",
        "XFH 应为客户端原始 Host"
    );
    assert!(v["connection"].is_null(), "Connection 应被剥:{v}");
    assert!(v["x-custom"].is_null(), "Connection 令牌头应被剥:{v}");
}

// ── Task 11:WebSocket 隧道 ─────────────────────────────────────

/// WS 回显上游:收一条回一条;连接关闭时置 AtomicBool(断开传播断言用)。
/// 注:oneshot::Sender 会让 axum Handler 对捕获闭包的推断失败(rustc 推断
/// 怪癖,String/Arc<AtomicBool> 均正常),故断开信号用共享原子标志。
async fn ws_echo_upstream(closed: Arc<std::sync::atomic::AtomicBool>) -> SocketAddr {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use std::sync::atomic::Ordering;

    async fn handle_socket(mut socket: WebSocket, closed: Arc<std::sync::atomic::AtomicBool>) {
        while let Some(Ok(msg)) = socket.recv().await {
            // 只回显文本/二进制(ping/pong/close 由协议层处理)
            if matches!(msg, Message::Text(_) | Message::Binary(_)) {
                let _ = socket.send(msg).await;
            }
        }
        closed.store(true, Ordering::SeqCst);
    }

    let c = closed.clone();
    let app = Router::new().route(
        "/ws",
        get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |socket| handle_socket(socket, c))
        }),
    );
    spawn_upstream(app).await
}

/// 意图:WebSocket 经代理握手、双向消息、断开传播到上游
/// (hyper::upgrade 隧道 + copy_bidirectional)。
#[tokio::test]
async fn proxies_websocket_echo() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let up = ws_echo_upstream(Arc::clone(&closed)).await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;

    // 直连代理地址但 Host 路由头指向 fs.opc.dongx.site
    let mut req = format!("ws://{proxy_addr}/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("host", "fs.opc.dongx.site".parse().unwrap());
    let (mut ws, resp) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(req),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status(), 101, "握手应经代理透传成功");

    for payload in ["hello", "world"] {
        ws.send(Message::Text(payload.into())).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(got.into_text().unwrap(), payload, "回显应一致");
    }

    // 断开传播:客户端 drop → 上游连接关闭 → 标志置位(5s 内轮询)
    drop(ws);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if closed.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        closed.load(std::sync::atomic::Ordering::SeqCst),
        "客户端断开应传播到上游"
    );
}
