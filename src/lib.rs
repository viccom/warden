//! warden —— 进程监护管理工具(库层)。
//!
//! 模块布局与设计见 docs/DESIGN.md,进度见 docs/ROADMAP.md。

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub mod api;
pub mod config;
pub mod error;
pub mod logs;
pub mod model;
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
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("[warden] Ctrl-C 收到,触发 shutdown");
            shutdown_tx.cancel();
        });
    }
    run_app_with_shutdown(cfg, config_path, shutdown).await
}

/// 核心:由外部传入 shutdown token(前台=Ctrl-C,Service=SCM Stop)。
/// init tracing → build state → start_auto → metrics → axum serve(graceful)→ stop_all。
pub async fn run_app_with_shutdown(
    cfg: config::Config,
    config_path: Option<PathBuf>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let bind = cfg.daemon.api_bind.clone();
    let _log_guard = init_tracing(&cfg.daemon.log_dir);
    tracing::info!(
        "[warden] {VERSION} 启动,配置 {} 个服务,api_bind={bind}",
        cfg.services.len()
    );

    let alert_webhook = cfg.daemon.alert_webhook.clone();
    let state = api::build_state(cfg, config_path);
    state.supervisor.start_auto().await;
    // 恢复期望状态:desired_state.json 中 true 的服务(auto_start 已启动的幂等跳过)
    state.supervisor.start_desired().await;
    state
        .supervisor
        .clone()
        .spawn_metrics(Duration::from_secs(2));
    // 健康检查 task(配了 health 的 Running 服务周期 TCP 探测 + 迁移告警)
    crate::supervisor::health::spawn_health(state.supervisor.clone(), alert_webhook);

    let supervisor = state.supervisor.clone();
    let app = api::build_router(state);
    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {bind} 失败:{e}"))?;
    tracing::info!("[warden] HTTP API listening on {bind}");

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
    tracing::info!("[warden] 已退出");
    Ok(())
}

/// 初始化 tracing:控制台层 + 可选按日轮转文件层。
/// 返回的 guard 须由调用方持有到程序结束(Service 模式在 exit(0) 前显式 drop)。
fn init_tracing(log_dir: &str) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, Layer};

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> =
        vec![fmt::layer().with_filter(filter.clone()).boxed()];

    let guard = if !log_dir.is_empty() {
        let appender = tracing_appender::rolling::daily(log_dir, "warden.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        layers.push(fmt::layer().with_writer(writer).with_filter(filter).boxed());
        Some(guard)
    } else {
        None
    };

    let _ = tracing_subscriber::registry().with(layers).try_init();
    guard
}
