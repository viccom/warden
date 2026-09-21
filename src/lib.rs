//! warden —— 进程监护管理工具(库层)。
//!
//! 模块布局与设计见 docs/DESIGN.md,进度见 docs/ROADMAP.md。

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub mod api;
pub mod config;
pub mod config_edit;
pub mod error;
pub mod lock;
pub mod logs;
pub mod model;
#[cfg(feature = "reverse-proxy")]
pub mod proxy;
pub mod service;
pub mod supervisor;
pub mod tui;

pub use error::{WResult, WardenError};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 前台运行 daemon(CLI `run`):加载配置 + Ctrl-C 触发 shutdown,委托 `run_app_with_shutdown`。
pub async fn run_app(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = match config::Config::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warden {VERSION}:配置加载失败:{e}");
            eprintln!(
                "用法:warden run --config <path>,或放置 config/services.toml(示例见 config/services.example.toml)"
            );
            return Ok(());
        }
    };
    let shutdown = CancellationToken::new();
    let shutdown_tx = shutdown.clone();
    // 前台:Ctrl-C 触发 graceful。
    // Windows:自有 console handler(CTRL_C/CTRL_BREAK 均拦截)——tokio ctrl_c 的接收端
    // 在首次事件后 drop,第二次 Ctrl-C 会被 std 默认 handler 以 0xC000013A 强杀,绕过 graceful。
    #[cfg(windows)]
    {
        if let Err(e) = supervisor::signal::install_console_shutdown(shutdown_tx) {
            tracing::warn!("[warden] 注册 console handler 失败,退回 tokio ctrl_c:{e}");
            let tx = shutdown.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("[warden] Ctrl-C 收到,触发 shutdown");
                tx.cancel();
            });
        }
    }
    #[cfg(not(windows))]
    {
        let tx_int = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("[warden] Ctrl-C 收到,触发 shutdown");
            tx_int.cancel();
        });
        // systemd stop / kill 发 SIGTERM:Unix 默认行为是立即终止进程——
        // 不显式监听会绕过优雅停机链(在途 HTTP 请求断开、被监护子进程只能靠
        // PDEATHSIG 被 SIGKILL 连带死)。与 ctrl_c 并行等价监听。
        let tx_term = shutdown_tx;
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("安装 SIGTERM 监听失败");
            sig.recv().await;
            tracing::info!("[warden] SIGTERM 收到,触发 shutdown");
            tx_term.cancel();
        });
    }
    run_app_with_shutdown(cfg, config_path, shutdown).await
}

/// 核心:由外部传入 shutdown token(前台=Ctrl-C,Service=SCM Stop)。
/// cwd 锚定 → init tracing → build state → start_auto → metrics → serve → stop_all。
pub async fn run_app_with_shutdown(
    cfg: config::Config,
    config_path: Option<PathBuf>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    // cwd 锚定:配置内相对路径(working_dir/command/data_dir/log_dir)统一锚定
    // 配置基准目录,与启动方式解耦——Service 模式 cwd=System32、用户从任意目录
    // `warden run` 时,`../xxx` 不再解析到部署树之外(桌面版 daemon::start 同款)。
    // 显式路径优先,否则按 find 链定位(build_state 内部同一解析,锚定后一致)。
    let cfg_file = config_path
        .clone()
        .or_else(config::Config::find_config_path);
    if let Some(base) = cfg_file.as_deref().and_then(config::config_base_dir) {
        if let Err(e) = std::env::set_current_dir(&base) {
            tracing::warn!(
                "[warden] cwd 锚定到 {} 失败({e}),相对路径按启动 cwd 解析",
                base.display()
            );
        }
    }
    let bind = cfg.daemon.api_bind.clone();
    let _log_guard = init_tracing(&cfg.daemon.log_dir);
    tracing::info!(
        "[warden] {VERSION} 启动,配置 {} 个服务,api_bind={bind}",
        cfg.services.len()
    );
    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {bind} 失败:{e}"))?;
    serve_with_shutdown(cfg, config_path, listener, shutdown).await
}

/// 共享 serve 编排(CLI 前台/Service/桌面版内嵌 daemon 同一实现):
/// build state → start_auto → metrics → health → serve(5s drain 上限)→ stop_all。
/// listener 由调用方绑定(CLI 用配置端口;桌面版用随机端口)。
pub async fn serve_with_shutdown(
    cfg: config::Config,
    config_path: Option<PathBuf>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    // D8:feature 未编译时 [proxy] 配置仅告警并忽略(在 cfg 被 build_state 消费前检测)
    #[cfg(not(feature = "reverse-proxy"))]
    if should_warn_proxy_ignored(&cfg) {
        tracing::warn!("[warden] reverse-proxy feature 未编译,配置中的 [proxy] 段将被忽略");
    }
    let alert_webhook = cfg.daemon.alert_webhook.clone();
    // [proxy] 段须在 cfg 被 build_state 消费前取出(引擎只在 feature 编译时拉起)
    #[cfg(feature = "reverse-proxy")]
    let proxy_launch = cfg.proxy.clone();
    let state = api::build_state(cfg, config_path);
    state.supervisor.start_auto().await;

    // 反向代理(P2):https(TLS 终止 + 证书热重载 + 到期检测)+ http
    // (转发,或 https 已启动时全量 301)。绑定失败不致命——监护/API 是
    // daemon 核心,反代是附加能力,降级为 error 日志继续。
    #[cfg(feature = "reverse-proxy")]
    let mut proxy_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    #[cfg(feature = "reverse-proxy")]
    if let Some(pc) = proxy_launch {
        use crate::proxy::{self, tls, HostRouter, HttpEntry, ProxyState};
        let empty = |s: &Option<String>| s.as_deref().map_or(true, str::is_empty);
        // https 入口先行(是否成功决定 http 是转发还是 301)
        let mut https_port: Option<u16> = None;
        let wants_https = !empty(&pc.https_bind) && !empty(&pc.cert_file) && !empty(&pc.key_file);
        if wants_https {
            let bind = pc.https_bind.clone().unwrap_or_default();
            let cert = std::path::PathBuf::from(pc.cert_file.clone().unwrap_or_default());
            let key = std::path::PathBuf::from(pc.key_file.clone().unwrap_or_default());
            let attempt = async {
                let listener = TcpListener::bind(&bind).await?;
                let port = listener.local_addr()?.port();
                let reloader = std::sync::Arc::new(
                    tls::CertReloader::new(&cert, &key).map_err(|e| anyhow::anyhow!(e))?,
                );
                Ok::<_, anyhow::Error>((listener, port, reloader))
            };
            match attempt.await {
                Ok((listener, port, reloader)) => {
                    let shared = state
                        .proxy_shared
                        .clone()
                        .expect("proxy_shared 与 [proxy] 段同生");
                    let state_tls = std::sync::Arc::new(ProxyState {
                        router: HostRouter::new(shared, state.supervisor.clone()),
                        client: proxy::build_client(&pc),
                        scheme: "https",
                        metrics: state.proxy_metrics.clone(),
                    });
                    proxy_tasks.push(tls::spawn_tls_serve(
                        listener,
                        reloader.clone(),
                        state_tls,
                        shutdown.clone(),
                    ));
                    proxy_tasks.push(tls::spawn_cert_reload(
                        reloader,
                        tls::CERT_RELOAD_PERIOD,
                        shutdown.clone(),
                    ));
                    // P3:到期检测(证书路径与 acme 配置;webhook 复用 daemon 告警)
                    proxy_tasks.push(tls::spawn_cert_expiry(
                        cert,
                        pc.acme.clone(),
                        alert_webhook.clone(),
                        shutdown.clone(),
                    ));
                    tracing::info!("[warden] 反代 https 已启动:{bind}");
                    https_port = Some(port);
                }
                Err(e) => {
                    tracing::error!("[warden] 反代 https 启动失败({bind}):{e:?}(降级 http 直转)");
                }
            }
        }
        match pc.http_bind.as_deref() {
            Some(bind) if !bind.is_empty() => match TcpListener::bind(bind).await {
                Ok(l) => {
                    let entry = match https_port {
                        Some(port) => HttpEntry::RedirectHttps { port },
                        None => {
                            let shared = state
                                .proxy_shared
                                .clone()
                                .expect("proxy_shared 与 [proxy] 段同生");
                            let state_http = std::sync::Arc::new(ProxyState {
                                router: HostRouter::new(shared, state.supervisor.clone()),
                                client: proxy::build_client(&pc),
                                scheme: "http",
                                metrics: state.proxy_metrics.clone(),
                            });
                            HttpEntry::Forward(state_http)
                        }
                    };
                    proxy_tasks.push(proxy::spawn_http(l, entry, shutdown.clone()));
                }
                Err(e) => {
                    tracing::error!("[warden] 反代监听 {bind} 绑定失败:{e}(忽略,继续监护/API)")
                }
            },
            _ if https_port.is_none() => tracing::warn!(
                "[warden] [proxy] 段存在但未配置 http_bind,http 不监听(https 见上方日志)"
            ),
            _ => {}
        }
    }
    state
        .supervisor
        .clone()
        .spawn_metrics(Duration::from_secs(2));
    // 健康检查 task(配了 health 的 Running 服务周期 TCP 探测 + 迁移告警)
    crate::supervisor::health::spawn_health(state.supervisor.clone(), alert_webhook);

    let supervisor = state.supervisor.clone();
    let app = api::build_router(state);
    tracing::info!(
        "[warden] HTTP API listening on {}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    );

    // shutdown 触发时停所有被监护服务(graceful),与 axum graceful 并行
    let stop_sup = supervisor.clone();
    let stop_shutdown = shutdown.clone();
    let stop_task = tokio::spawn(async move {
        stop_shutdown.cancelled().await;
        tracing::info!("[warden] shutdown 信号,停止所有被监护服务...");
        stop_sup.stop_all().await;
    });

    // graceful shutdown 设上限:SSE / 日志流等长连接不会主动断开,
    // 无限等待会卡住退出(实测 UI 页面开着时 Ctrl-C 后进程不退出)。
    // 注意:上限从 shutdown 触发后起算(先 await cancelled 再 sleep),否则会变成启动 5s 必退。
    let serve_shutdown = shutdown.clone();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        serve_shutdown.cancelled().await;
    });
    let drain_limit = shutdown.clone();
    tokio::select! {
        r = serve => r.map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?,
        _ = async move {
            drain_limit.cancelled().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        } => {
            tracing::warn!("[warden] HTTP 连接 5s 内未全部关闭(长连接),强制断开");
        }
    }

    // 等 stop_all 完成(子进程 graceful 收尾),再退出
    let _ = stop_task.await;
    // 反代 drain(15s 上限)独立于 API 5s:各组并行等待,总退出 = max(各链)
    // (设计 §4.5——不 await 会随主流程退出被 runtime 硬杀,在途代理连接/WS
    //  隧道实际只剩 API 的 5s 上限,与设计相悖)
    #[cfg(feature = "reverse-proxy")]
    for t in proxy_tasks {
        let _ = t.await;
    }
    tracing::info!("[warden] 已退出");
    Ok(())
}

/// 初始化 tracing:控制台层 + 可选按日轮转文件层(warden.log 全量 +
/// proxy-access.log 反代访问日志,P5 落盘轮转)。
/// 返回的 guard 须由调用方持有到程序结束(Service 模式在 exit(0) 前显式 drop;
/// 桌面版存于 EmbeddedDaemon)。guard 用包装类型,调用方(桌面 crate)无需
/// 依赖 tracing_appender。纯 RAII 守卫:字段仅靠 Drop 时 flush 生效,永不被读。
#[expect(dead_code)]
pub struct LogGuard(Vec<tracing_appender::non_blocking::WorkerGuard>);

pub fn init_tracing(log_dir: &str) -> LogGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, Layer};

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> =
        vec![fmt::layer().with_filter(filter.clone()).boxed()];

    let mut guards = Vec::new();
    if !log_dir.is_empty() {
        let appender = tracing_appender::rolling::daily(log_dir, "warden.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        layers.push(fmt::layer().with_writer(writer).with_filter(filter).boxed());
        guards.push(guard);
        // 反代 access log 独立文件(按日轮转):只收 proxy_access target
        // (forward.rs 的 access 行),控制台层不受影响照常输出
        let access_appender = tracing_appender::rolling::daily(log_dir, "proxy-access.log");
        let (access_writer, access_guard) = tracing_appender::non_blocking(access_appender);
        let access_filter = tracing_subscriber::filter::Targets::new()
            .with_target("proxy_access", tracing::Level::INFO);
        layers.push(
            fmt::layer()
                .with_writer(access_writer)
                .with_target(false)
                .with_level(false)
                .with_file(false)
                .with_line_number(false)
                .with_filter(access_filter)
                .boxed(),
        );
        guards.push(access_guard);
    }

    let _ = tracing_subscriber::registry().with(layers).try_init();
    LogGuard(guards)
}

#[cfg(not(feature = "reverse-proxy"))]
#[test]
fn proxy_module_absent_without_feature() {
    // 编译期锚点:无 feature 时本测试参与编译,而 warden::proxy 被 cfg 掉。
    // 若有人误删 lib.rs 的 #[cfg(feature)],无 feature 形态将在编译 proxy 模块
    // (P1 起引用 hyper 等 optional 依赖)时失败。行为验证由 config 单测承担
    // (不依赖 proxy mod,双形态共用)。
}

/// feature 未编译时,判定 [proxy] 配置是否会因 feature 缺失而被忽略(D8)。
/// serve_with_shutdown 启动时据此 warn;纯函数供 e2e 断言。
#[cfg(not(feature = "reverse-proxy"))]
pub fn should_warn_proxy_ignored(cfg: &config::Config) -> bool {
    cfg.proxy.is_some()
}
