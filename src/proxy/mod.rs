//! 反向代理模块。设计见 docs/PLAN-REVERSE-PROXY.md。
//!
//! 编排(`spawn`):从 [proxy] 配置构建 HostRouter 与 hyper-util 连接池客户端,
//! 绑定监听器,挂 CancellationToken 优雅停机(drain 上限 15s,覆盖 WS 隧道强关;
//! API 侧维持 5s 不变,两组并行,见设计 §4.5)。

pub mod forward;
pub mod router;

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
use router::HostRouter;

/// 代理引擎共享状态(handler 每请求经 Arc 读取)。
pub struct ProxyState {
    pub router: HostRouter,
    /// hyper-util legacy client(连接池,HTTP/1.1;流式直传,见 D5)。
    /// body 泛型固定 axum::body::Body(请求原样透传,零重组)。
    pub client: Client<HttpConnector, axum::body::Body>,
    /// 客户端访问 scheme(P1 恒 "http";P2 起 https 终止后按监听器传 "https")。
    pub scheme: &'static str,
}

/// 优雅停机时在途连接(含 WS 隧道)的 drain 上限。
const DRAIN_LIMIT: Duration = Duration::from_secs(15);

/// 启动反代服务(后台 task):绑定已就绪的 listener,shutdown 触发后
/// 停止接受新连接 → 在途请求 drain(15s 上限强断)。
/// 返回 JoinHandle 供编排方等待退出。
pub fn spawn(
    cfg: ProxyConfig,
    supervisor: Arc<Supervisor>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let connect_timeout = Duration::from_millis(cfg.connect_timeout_ms);
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(connect_timeout));
        let client: Client<HttpConnector, axum::body::Body> =
            Client::builder(TokioExecutor::new()).build(connector);
        let scheme = "http"; // P2 起 https listener 传 "https"
        let state = Arc::new(ProxyState {
            router: HostRouter::new(cfg, supervisor),
            client,
            scheme,
        });
        let app = axum::Router::new()
            .fallback(forward::proxy_handler)
            .with_state(state)
            .into_make_service_with_connect_info::<SocketAddr>();
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        tracing::info!("[proxy] listening on {local}");
        let serve_shutdown = shutdown.clone();
        let serve = axum::serve(listener, app)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await });
        // 与 API 侧同款 drain 模式:上限从 shutdown 触发后起算(防"启动即自杀")
        let drain = shutdown;
        tokio::select! {
            r = serve => {
                if let Err(e) = r {
                    tracing::error!("[proxy] serve 退出:{e}");
                }
            }
            _ = async move {
                drain.cancelled().await;
                tokio::time::sleep(DRAIN_LIMIT).await;
            } => {
                tracing::warn!("[proxy] 连接 {DRAIN_LIMIT:?} 内未全部关闭(WS 隧道/在途请求),强制断开");
            }
        }
        tracing::info!("[proxy] 已退出");
    })
}

// 编译期断言:hyper/hyper-util 在本 feature 下作为依赖可见。
// (计划原文的 `Client::builder` 函数指针写法在本版本编译不过——泛型关联函数
//  无法 coerce 成非泛型 fn 指针,且 Exec 未公开 re-export;断言改为公开类型,
//  Client 构造的真实可用性由 forward.rs 实现与 e2e 验证。)
const _: fn() = || {
    fn _assert_hyper() -> hyper::Request<hyper::body::Incoming> {
        unreachable!()
    }
    fn _assert_hyper_util() -> hyper_util::client::legacy::connect::HttpConnector {
        unreachable!()
    }
};
