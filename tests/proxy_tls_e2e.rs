#![cfg(feature = "reverse-proxy")]
//! P2 TLS e2e:rustls 终止 + 证书热重载 + HTTP→HTTPS 301 + https 上游(自定义 CA)。
//!
//! 证书全由 rcgen 现场生成(CA → 签发叶子),客户端用自定义根的 rustls 连接,
//! 无外部网络依赖。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::Service; // Router::call
use warden::config::{ProxyConfig, ProxyRoute};
use warden::proxy::tls::{spawn_cert_reload, spawn_tls_serve, CertReloader};
use warden::proxy::{shared_from, HostRouter, HttpEntry, ProxyMetrics, ProxyState};
use warden::supervisor::Supervisor;

const DOMAIN: &str = "opc.dongx.site";

// ── 证书生成(rcgen:CA → 叶子)──────────────────────────────────

struct TestCa {
    params: CertificateParams,
    key: KeyPair,
    cert: rcgen::Certificate,
}

/// 生成测试 CA(is_ca + 约束深度 0,根证书可作信任锚)。
fn new_ca() -> TestCa {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    TestCa { params, key, cert }
}

/// 用 CA 签发叶子证书;返回 (cert_pem, key_pem)。sans 为 DNS SAN。
fn issue_dns(ca: &TestCa, sans: &[&str]) -> (String, String) {
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans
        .iter()
        .map(|s| rcgen::SanType::DnsName(s.to_string().try_into().unwrap()))
        .collect();
    issue(ca, params)
}

/// 用 CA 签发带 IP SAN 的叶子(https 上游监听 127.0.0.1)。
fn issue_ip(ca: &TestCa) -> (String, String) {
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
    issue(ca, params)
}

fn issue(ca: &TestCa, params: CertificateParams) -> (String, String) {
    let leaf_key = KeyPair::generate().unwrap();
    let issuer = Issuer::new(ca.params.clone(), &ca.key);
    let cert = params.signed_by(&leaf_key, &issuer).unwrap();
    (cert.pem(), leaf_key.serialize_pem())
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("warden-tls-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ── rustls 客户端(自定义信任锚,走 hyper http1 conn)──────────────

/// 以给定 PEM(信任锚)发起 https GET;返回 (status, body) 或握手/请求错误文案。
async fn tls_get(addr: SocketAddr, host: &str, root_pem: &str) -> Result<(u16, String), String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(root_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let sni = ServerName::try_from(host.to_owned()).map_err(|e| e.to_string())?;
    let tls = connector
        .connect(sni, tcp)
        .await
        .map_err(|e| format!("tls: {e}"))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| format!("hs: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender
        .send_request(
            hyper::Request::builder()
                .uri(format!("https://{host}/"))
                .header("host", host)
                .body(http_body_util::Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .map_err(|e| format!("req: {e}"))?;
    let status = resp.status().as_u16();
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .map_err(|e| format!("body: {e}"))?
        .to_bytes();
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

// ── 引擎装配(直接构建 ProxyState,绑 https/http 监听)────────────

fn base_cfg(routes: Vec<ProxyRoute>) -> ProxyConfig {
    ProxyConfig {
        domain: Some(DOMAIN.into()),
        http_bind: Some("127.0.0.1:0".into()),
        https_bind: None,
        connect_timeout_ms: 2000,
        preserve_host: false,
        cert_file: None,
        key_file: None,
        upstream_ca_file: None,
        acme: Default::default(),
        routes,
    }
}

fn route(host: &str, to: String) -> ProxyRoute {
    ProxyRoute {
        host: host.into(),
        to: Some(to),
        service: None,
        preserve_host: None,
    }
}

/// 构建 https 侧 ProxyState(scheme=https)+ 共享配置句柄。
fn tls_state(cfg: &ProxyConfig) -> (Arc<ProxyState>, warden::proxy::SharedProxyConfig) {
    let shared = shared_from(cfg.clone());
    let state = Arc::new(ProxyState {
        router: HostRouter::new(shared.clone(), Arc::new(Supervisor::new(PathBuf::from("")))),
        client: warden::proxy::build_client(cfg),
        scheme: "https",
        metrics: Arc::new(ProxyMetrics::new()),
    });
    (state, shared)
}

async fn bind_local() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

// ── 用例 ────────────────────────────────────────────────────────

/// 意图:TLS 终止基本链路——https 经代理路由到上游,通配证书匹配子域,
/// 客户端以签发 CA 为信任锚验证通过(X-Forwarded-Proto=https 同步验证)。
#[tokio::test]
async fn serves_https_with_tls_termination() {
    let up = spawn_plain_upstream().await;
    let ca = new_ca();
    let (cert, key) = issue_dns(&ca, &[&format!("*.{DOMAIN}")]);
    let dir = tmpdir("serve");
    let cert_path = dir.join("fullchain.pem");
    let key_path = dir.join("privkey.pem");
    std::fs::write(&cert_path, &cert).unwrap();
    std::fs::write(&key_path, &key).unwrap();

    let cfg = base_cfg(vec![route(&format!("fs.{DOMAIN}"), format!("http://{up}"))]);
    let (state, _shared) = tls_state(&cfg);
    let reloader = Arc::new(CertReloader::new(&cert_path, &key_path).unwrap());
    let (listener, addr) = bind_local().await;
    let shutdown = CancellationToken::new();
    spawn_tls_serve(listener, reloader, state, shutdown.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (status, body) = tls_get(addr, &format!("fs.{DOMAIN}"), &ca.cert.pem())
        .await
        .expect("CA 信任的叶子证书应握手成功");
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body, "upstream-ok");
    shutdown.cancel();
}

async fn spawn_plain_upstream() -> SocketAddr {
    let app = Router::new().route("/", get(|| async { "upstream-ok" }));
    let (l, a) = bind_local().await;
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    a
}

/// 意图:证书换证(mtime 变化)后 acceptor 热生效——旧信任锚握手失败、
/// 新信任锚成功,证明无需重启即换证书(为续签不停机铺路)。
#[tokio::test]
async fn reloads_certificate_on_change() {
    let up = spawn_plain_upstream().await;
    let ca_a = new_ca();
    let ca_b = new_ca();
    let (cert_a, key_a) = issue_dns(&ca_a, &[&format!("*.{DOMAIN}")]);
    let dir = tmpdir("reload");
    let cert_path = dir.join("fullchain.pem");
    let key_path = dir.join("privkey.pem");
    std::fs::write(&cert_path, &cert_a).unwrap();
    std::fs::write(&key_path, &key_a).unwrap();

    let cfg = base_cfg(vec![route(&format!("fs.{DOMAIN}"), format!("http://{up}"))]);
    let (state, _) = tls_state(&cfg);
    let reloader = Arc::new(CertReloader::new(&cert_path, &key_path).unwrap());
    let (listener, addr) = bind_local().await;
    let shutdown = CancellationToken::new();
    spawn_tls_serve(listener, reloader.clone(), state, shutdown.clone());
    // 100ms 轮询(生产 30s,测试压缩)
    spawn_cert_reload(
        reloader.clone(),
        Duration::from_millis(100),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 基线:CA-A 信任
    assert!(tls_get(addr, &format!("fs.{DOMAIN}"), &ca_a.cert.pem())
        .await
        .is_ok());

    // 换证:落盘 CA-B 签发的证书对
    let (cert_b, key_b) = issue_dns(&ca_b, &[&format!("*.{DOMAIN}")]);
    std::fs::write(&cert_path, &cert_b).unwrap();
    std::fs::write(&key_path, &key_b).unwrap();

    // 等轮询周期(留足余量:两轮 mtime 检查)
    tokio::time::sleep(Duration::from_millis(600)).await;

    let old = tls_get(addr, &format!("fs.{DOMAIN}"), &ca_a.cert.pem()).await;
    assert!(old.is_err(), "旧信任锚应失败(证书已换):{old:?}");
    let new = tls_get(addr, &format!("fs.{DOMAIN}"), &ca_b.cert.pem()).await;
    assert!(new.is_ok(), "新信任锚应成功:{new:?}");
    shutdown.cancel();
}

/// 意图:80/443 同配时 http 入口全量 301 到 https(Host 保留、附非标端口、
/// path+query 原样);畸形 Host 不进 Location(400)。
#[tokio::test]
async fn redirects_http_to_https() {
    let (listener, addr) = bind_local().await;
    let shutdown = CancellationToken::new();
    warden::proxy::spawn_http(
        listener,
        HttpEntry::RedirectHttps { port: 8443 },
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 标准端口形态:Location 不带端口
    let (l443, a443) = bind_local().await;
    warden::proxy::spawn_http(
        l443,
        HttpEntry::RedirectHttps { port: 443 },
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = no_redirect
        .get(format!("http://{a443}/x/y?q=1"))
        .header("host", format!("fs.{DOMAIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        301,
        "body: {}",
        resp.text().await.unwrap_or_default()
    );
    assert_eq!(
        resp.headers().get("location").unwrap(),
        &format!("https://fs.{DOMAIN}/x/y?q=1"),
        "443 端口不附加 :443"
    );

    // 非标端口形态:Location 带端口
    let resp = no_redirect
        .get(format!("http://{addr}/"))
        .header("host", format!("fs.{DOMAIN}:8080"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 301);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        &format!("https://fs.{DOMAIN}:8443/"),
        "请求 Host 的入站端口剥离,目标端口取 https 配置"
    );

    // 畸形 Host 不重定向(防 Host 注入 Location)
    let resp = no_redirect
        .get(format!("http://{addr}/"))
        .header("host", "evil host/.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "非法 Host 字符直接 400");
    shutdown.cancel();
}

/// 意图:https 上游经 hyper-rustls——自定义 CA(upstream_ca_file)时全链路 200;
/// 未配置该 CA 时验证失败 → 502(不 panic/不挂起)。
#[tokio::test]
async fn https_upstream_via_custom_ca() {
    let up_ca = new_ca();
    let (up_cert, up_key) = issue_ip(&up_ca);
    let dir = tmpdir("upstream");
    let ca_path = dir.join("ca.pem");
    std::fs::write(&ca_path, up_ca.cert.pem()).unwrap();
    let up_addr = spawn_tls_upstream(&up_cert, &up_key).await;

    // 直连自检:CA 与叶子证书本身可用(与代理解耦,失败即证书问题非引擎问题)
    let direct = tls_get(up_addr, "127.0.0.1", &up_ca.cert.pem()).await;
    assert!(direct.is_ok(), "直连 https 上游应通过:{direct:?}");

    // 正向:配置自定义 CA
    let mut cfg = base_cfg(vec![route(
        &format!("fs.{DOMAIN}"),
        format!("https://{up_addr}"),
    )]);
    cfg.upstream_ca_file = Some(ca_path.to_string_lossy().into_owned());
    let state = Arc::new(ProxyState {
        router: HostRouter::new(
            shared_from(cfg.clone()),
            Arc::new(Supervisor::new(PathBuf::from(""))),
        ),
        client: warden::proxy::build_client(&cfg),
        scheme: "http",
        metrics: Arc::new(ProxyMetrics::new()),
    });
    let (listener, addr) = bind_local().await;
    let shutdown = CancellationToken::new();
    warden::proxy::spawn_http(listener, HttpEntry::Forward(state), shutdown.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header("host", format!("fs.{DOMAIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "自定义 CA 应信任 https 上游;body: {}",
        resp.text().await.unwrap_or_default()
    );

    // 反向:同一上游,不配 CA(webpki 根不认测试 CA)→ 502
    let cfg2 = base_cfg(vec![route(
        &format!("fs.{DOMAIN}"),
        format!("https://{up_addr}"),
    )]);
    let shared2 = shared_from(cfg2.clone());
    let state2 = Arc::new(ProxyState {
        router: HostRouter::new(shared2, Arc::new(Supervisor::new(PathBuf::from("")))),
        client: warden::proxy::build_client(&cfg2),
        scheme: "http",
        metrics: Arc::new(ProxyMetrics::new()),
    });
    let (l2, a2) = bind_local().await;
    warden::proxy::spawn_http(l2, HttpEntry::Forward(state2), shutdown.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{a2}/"))
        .header("host", format!("fs.{DOMAIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502, "未知 CA 的 https 上游应 502");
    shutdown.cancel();
}

/// 测试用 https 上游:axum over tokio-rustls(http1)。
async fn spawn_tls_upstream(cert_pem: &str, key_pem: &str) -> SocketAddr {
    let cfg = load_server_config_from_pem(cert_pem, key_pem);
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let (l, a) = bind_local().await;
    tokio::spawn(async move {
        let app = Router::new().route("/", get(|| async { "upstream-ok" }));
        loop {
            let Ok((stream, _)) = l.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let svc = hyper::service::service_fn(move |req| {
                    let mut svc = app.clone();
                    async move { svc.call(req).await }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), svc)
                    .await;
            });
        }
    });
    a
}

fn load_server_config_from_pem(cert_pem: &str, key_pem: &str) -> rustls::ServerConfig {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<_> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap()
}
