#![cfg(feature = "reverse-proxy")]
//! 反向代理 e2e。本地上游用 tokio::spawn + axum 随机端口,不固定端口。

use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use warden::config::{ProxyConfig, ProxyRoute};
use warden::supervisor::Supervisor;

/// 起一个本地上游(随机端口),GET / 返回固定 body。
async fn plain_upstream() -> std::net::SocketAddr {
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "upstream-ok" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// 起代理:一条显式路由 fs.opc.dongx.site → upstream,返回 (代理地址, shutdown)。
async fn spawn_proxy(routes: Vec<ProxyRoute>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = ProxyConfig {
        domain: Some("opc.dongx.site".into()),
        http_bind: Some("127.0.0.1:0".into()),
        https_bind: None,
        connect_timeout_ms: 2000,
        preserve_host: false,
        routes,
    };
    let supervisor = Arc::new(Supervisor::new(std::path::PathBuf::from("")));
    let shutdown = CancellationToken::new();
    warden::proxy::spawn(cfg, supervisor, listener, shutdown);
    addr
}

fn route(host: &str, to: String) -> ProxyRoute {
    ProxyRoute {
        host: host.into(),
        to: Some(to),
        service: None,
        preserve_host: None,
    }
}

#[tokio::test]
async fn proxies_get_body() {
    let up = plain_upstream().await;
    let proxy_addr = spawn_proxy(vec![route("fs.opc.dongx.site", format!("http://{up}"))]).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .header("host", "fs.opc.dongx.site")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "upstream-ok");
}
