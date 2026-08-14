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
pub mod supervisor;

pub use error::{WardenError, WResult};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 前台运行 daemon:加载配置 → tracing → 启动 auto_start → metrics 采样 → HTTP API → graceful shutdown。
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
    let bind = cfg.daemon.api_bind.clone();
    let _log_guard = init_tracing(&cfg.daemon.log_dir);

    tracing::info!(
        "[warden] {VERSION} 启动,配置 {} 个服务,api_bind={bind}",
        cfg.services.len()
    );

    let state = api::build_state(cfg, config_path);
    // 拉起 auto_start 服务
    state.supervisor.start_auto().await;
    // 后台 metrics 采样
    state.supervisor.clone().spawn_metrics(Duration::from_secs(2));

    let supervisor = state.supervisor.clone();
    let app = api::build_router(state);

    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {bind} 失败:{e}"))?;
    tracing::info!("[warden] HTTP API listening on {bind}(Ctrl-C 退出)");

    // Ctrl-C → 停所有服务 → 触发 axum graceful shutdown
    let shutdown = CancellationToken::new();
    let shutdown_tx = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("[warden] 收到 Ctrl-C,停止所有服务...");
            supervisor.stop_all().await;
            shutdown_tx.cancel();
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?;

    tracing::info!("[warden] 已退出");
    Ok(())
}

/// 初始化 tracing:控制台层 + 可选按日轮转文件层。
/// 返回的 guard 须由调用方持有到程序结束(否则丢末尾日志)。
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
