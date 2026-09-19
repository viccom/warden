//! 反向代理模块。设计见 docs/PLAN-REVERSE-PROXY.md。
//!
//! 编排:`spawn`(单 http 直转,e2e/简单部署)/调用方自行组合
//! `spawn_http` + `tls::spawn_tls_serve`(P2 双入口:http 可 301 → https)。
//! 路由表经 [SharedProxyConfig] 共享——reload/CRUD 写、每请求读,热生效。

pub mod forward;
pub mod router;
pub mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::config::ProxyConfig;
use crate::supervisor::Supervisor;
pub use router::HostRouter;

/// 路由热更新的共享配置:reload/路由 CRUD 写(read 拷 Arc 快照,每请求开销极低)。
pub type SharedProxyConfig = Arc<std::sync::RwLock<Arc<ProxyConfig>>>;

pub fn shared_from(cfg: ProxyConfig) -> SharedProxyConfig {
    Arc::new(std::sync::RwLock::new(Arc::new(cfg)))
}

/// 优雅停机时在途连接(含 WS 隧道)的 drain 上限。
pub const DRAIN_LIMIT: Duration = Duration::from_secs(15);

/// 代理引擎共享状态(handler 每请求经 Arc 读取)。
pub struct ProxyState {
    pub router: HostRouter,
    /// hyper-util legacy client(连接池,HTTP/1.1;流式直传,见 D5)。
    /// 上游 http/https 均经此连接器(P2:https 由 hyper-rustls 终止)。
    pub client: Client<hyper_rustls::HttpsConnector<HttpConnector>, axum::body::Body>,
    /// 客户端访问 scheme(http 入口 "http";https 入口 "https")。
    pub scheme: &'static str,
    /// 路由级计数(P5):按已路由 host 键聚合请求数/5xx 数,API 透出。
    pub metrics: Arc<ProxyMetrics>,
}

/// 路由级 metrics(P5):DashMap 按已路由 host 聚合,写侧为原子自增。
/// 只记录已路由请求(NotFound/坏 Host 不计数,防恶意 Host 撑爆键空间)。
#[derive(Default)]
pub struct ProxyMetrics {
    inner: dashmap::DashMap<String, RouteCounter>,
}

/// 单路由计数快照(serde 序列化由 API 层组装)。
#[derive(Default)]
pub struct RouteCounter {
    pub requests: std::sync::atomic::AtomicU64,
    pub server_errors: std::sync::atomic::AtomicU64,
}

impl ProxyMetrics {
    pub fn new() -> Self {
        Self {
            inner: dashmap::DashMap::new(),
        }
    }

    /// 记录一次已路由请求(status 为响应码;5xx 计入错误)。
    pub fn record(&self, route: &str, status: u16) {
        use std::sync::atomic::Ordering::Relaxed;
        let e = self.inner.entry(route.to_owned()).or_default();
        e.requests.fetch_add(1, Relaxed);
        if (500..600).contains(&status) {
            e.server_errors.fetch_add(1, Relaxed);
        }
    }

    /// 全量快照(按请求数降序,便于 UI 呈现)。
    pub fn snapshot(&self) -> Vec<(String, u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut v: Vec<_> = self
            .inner
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    e.requests.load(Relaxed),
                    e.server_errors.load(Relaxed),
                )
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

/// http 入口形态:正常转发,或(80/443 同配时)全量 301 到 https。
pub enum HttpEntry {
    Forward(Arc<ProxyState>),
    /// `port`:https 监听端口(443 时 Location 不附端口)。
    RedirectHttps {
        port: u16,
    },
}

/// 构建上游客户端:http/https 均可(自定义 CA 见 `upstream_ca_file`)。
pub fn build_client(
    cfg: &ProxyConfig,
) -> Client<hyper_rustls::HttpsConnector<HttpConnector>, axum::body::Body> {
    let mut connector = HttpConnector::new();
    // scheme 由 HttpsConnector 分派(http 直连/https 包 TLS);内层强制 http 会
    // 在 https 请求进到 TLS 层之前就拒绝(hyper-rustls 自建内层同款设 false)
    connector.enforce_http(false);
    connector.set_connect_timeout(Some(Duration::from_millis(cfg.connect_timeout_ms)));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(upstream_tls_config(cfg))
        .https_or_http()
        .enable_http1()
        .wrap_connector(connector);
    Client::builder(TokioExecutor::new()).build(https)
}

/// 上游 TLS 信任:webpki 内置根 + 可选自定义 CA(内网私有 CA 场景)。
fn upstream_tls_config(cfg: &ProxyConfig) -> rustls::ClientConfig {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(ca_file) = cfg.upstream_ca_file.as_deref().filter(|s| !s.is_empty()) {
        match rustls::pki_types::CertificateDer::pem_file_iter(ca_file) {
            Ok(iter) => {
                let mut n = 0;
                for c in iter.flatten() {
                    let _ = roots.add(c);
                    n += 1;
                }
                if n == 0 {
                    tracing::warn!("[proxy] upstream_ca_file '{ca_file}' 无 PEM 证书,仅用内置根");
                }
            }
            Err(e) => {
                tracing::warn!("[proxy] upstream_ca_file '{ca_file}' 读取失败({e}),仅用内置根")
            }
        }
    }
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// 便捷入口:单 http 直转监听(e2e/简单部署;生产编排走 spawn_http + tls)。
pub fn spawn(
    cfg: ProxyConfig,
    supervisor: Arc<Supervisor>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let client = build_client(&cfg);
    let shared = shared_from(cfg);
    let state = Arc::new(ProxyState {
        router: HostRouter::new(shared, supervisor),
        client,
        scheme: "http",
        metrics: Arc::new(ProxyMetrics::new()),
    });
    spawn_http(listener, HttpEntry::Forward(state), shutdown)
}

/// 启动 http 入口(后台 task):转发或 301 → https,shutdown 后 drain(15s 上限)。
pub fn spawn_http(
    listener: TcpListener,
    entry: HttpEntry,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        match entry {
            HttpEntry::Forward(state) => {
                tracing::info!("[proxy] http listening on {local}");
                let router = axum::Router::new()
                    .fallback(forward::proxy_handler)
                    .with_state(state);
                serve_and_drain(listener, router, shutdown, "http").await;
            }
            HttpEntry::RedirectHttps { port } => {
                tracing::info!("[proxy] http listening on {local}(301 → https:{port})");
                let router = axum::Router::new()
                    .fallback(forward::redirect_to_https)
                    .with_state(port);
                serve_and_drain(listener, router, shutdown, "http(redirect)").await;
            }
        }
    })
}

/// serve + drain 公共模式(与 API 侧同款:上限从 shutdown 触发后起算,
/// 防"启动即自杀";见设计 §4.5)。
pub(crate) async fn serve_and_drain(
    listener: TcpListener,
    router: axum::Router,
    shutdown: CancellationToken,
    tag: &str,
) {
    let app = router.into_make_service_with_connect_info::<SocketAddr>();
    let serve_shutdown = shutdown.clone();
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(async move { serve_shutdown.cancelled().await });
    let drain = shutdown;
    tokio::select! {
        r = serve => {
            if let Err(e) = r {
                tracing::error!("[proxy] {tag} serve 退出:{e}");
            }
        }
        _ = async move {
            drain.cancelled().await;
            tokio::time::sleep(DRAIN_LIMIT).await;
        } => {
            tracing::warn!("[proxy] {tag} 连接 {DRAIN_LIMIT:?} 内未全部关闭(WS 隧道/在途请求),强制断开");
        }
    }
    tracing::info!("[proxy] {tag} 已退出");
}

// 编译期断言:TLS/上游 https 依赖在本 feature 下可见(真实可用性由
// tls.rs 实现与 proxy_tls_e2e 验证)。
const _: fn() = || {
    fn _assert_rustls() -> rustls::ClientConfig {
        unreachable!()
    }
    fn _assert_tokio_rustls() -> tokio_rustls::TlsAcceptor {
        unreachable!()
    }
    fn _assert_hyper() -> hyper::Request<hyper::body::Incoming> {
        unreachable!()
    }
    fn _assert_hyper_util() -> hyper_util::client::legacy::connect::HttpConnector {
        unreachable!()
    }
};
