#![cfg(feature = "reverse-proxy")]
//! P5 access log 落盘轮转 e2e:真实 daemon(run_app_with_shutdown)+
//! log_dir 配置 → 代理请求 → proxy-access.log.<date> 含 access 行。
//! 独立测试文件(专进程):init_tracing 的全局 subscriber 只装一次。

use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
use warden::config::Config;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn access_log_written_to_rotating_file() {
    let d = std::env::temp_dir().join(format!("warden-acclog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("logs")).unwrap();

    // 本地上游
    let up = {
        let app = Router::new().route("/", get(|| async { "up-ok" }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        a
    };

    let api_port = free_port();
    let px_port = free_port();
    let tls_port = free_port();
    let cfg_path = d.join("services.toml");
    // 自签通配证书(CertReloader 只消费 PEM,自签即可)
    let ck = rcgen::generate_simple_self_signed(vec!["*.x.example.com".to_string()]).unwrap();
    let cert_path = d.join("fullchain.pem");
    let key_path = d.join("privkey.pem");
    std::fs::write(&cert_path, ck.cert.pem()).unwrap();
    std::fs::write(&key_path, ck.signing_key.serialize_pem()).unwrap();
    std::fs::write(
        &cfg_path,
        format!(
            r#"[daemon]
api_bind = "127.0.0.1:{api_port}"
auth_token = ""
data_dir = ""
log_dir = "{logs}"

[proxy]
domain = "x.example.com"
http_bind = "127.0.0.1:{px_port}"
https_bind = "127.0.0.1:{tls_port}"
cert_file = "{cert}"
key_file = "{key}"

[[proxy.route]]
host = "fs.x.example.com"
to = "http://{up}"
"#,
            logs = d.join("logs").to_string_lossy().replace('\\', "/"),
            cert = cert_path.to_string_lossy().replace('\\', "/"),
            key = key_path.to_string_lossy().replace('\\', "/"),
        ),
    )
    .unwrap();
    let cfg = Config::load(Some(&cfg_path)).unwrap();

    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let daemon =
        tokio::spawn(
            async move { warden::run_app_with_shutdown(cfg, Some(cfg_path), cancel).await },
        );

    // 就绪:http 入口应 301(https 同配时全量重定向)
    let url = format!("http://127.0.0.1:{px_port}/");
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut redirected = false;
    for _ in 0..50 {
        if let Ok(r) = no_redirect
            .get(&url)
            .header("host", "fs.x.example.com")
            .send()
            .await
        {
            if r.status() == 301 {
                redirected = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(redirected, "http 入口应 301 → https");
    // 再打一次(累计两行 301)
    let _ = no_redirect
        .get(&url)
        .header("host", "fs.x.example.com")
        .send()
        .await
        .unwrap();

    shutdown.cancel();
    let _ = daemon.await;

    // 落盘断言:proxy-access.log.<date>(按日轮转命名)含 access 行 ×2
    let mut found = false;
    for _ in 0..20 {
        let entries: Vec<_> = std::fs::read_dir(d.join("logs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("proxy-access.log")
            })
            .collect();
        if let Some(f) = entries.first() {
            let text = std::fs::read_to_string(f.path()).unwrap();
            let hits = text.matches("GET fs.x.example.com/ 301").count();
            if hits >= 2 {
                found = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found,
        "proxy-access.log 应含 2 条 GET fs.x.example.com/ 301 行(重定向流量也须落 access log)"
    );
    let _ = std::fs::remove_dir_all(&d);
}
