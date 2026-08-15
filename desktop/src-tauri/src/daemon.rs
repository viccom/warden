//! 内嵌 daemon:桌面版进程内跑 warden 监护引擎 + HTTP API。
//!
//! 与 CLI `run` 的差异:
//! - 绑 `127.0.0.1:0` 随机端口(避免与用户手跑的 CLI warden 8789 冲突),
//!   实际端口 + 随机 token 经 Tauri 命令交给前端
//! - data_dir/log_dir 重定向到桌面应用数据目录(与 CLI 隔离,
//!   防止两个 daemon 同写 desired_state.json / runtime overlay)
//! - 无 console 信号处理(退出由托盘/命令触发 shutdown token)
//! - 配置缺失不退出(空配置起步,服务经 CRUD 添加)

use std::path::Path;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use warden::api::{build_router, build_state};

/// 起内嵌 daemon(阻塞直到 listener 就绪),返回端口/token 与停止句柄。
pub fn start(app_data: &Path) -> anyhow::Result<EmbeddedDaemon> {
    // 配置:复用 CLI 路径规则($WARDEN_CONFIG → exe_dir → cwd → 平台位置);缺配置则空集
    let mut cfg = warden::config::Config::load(None).unwrap_or_else(|e| {
        eprintln!("[warden-desktop] 配置加载失败({e}),以空配置启动(可经界面 CRUD 添加服务)");
        warden::config::Config::default()
    });
    // 桌面隔离:数据/日志独立目录,鉴权用进程内随机 token
    let data_dir = app_data.join("data");
    std::fs::create_dir_all(&data_dir)?;
    cfg.daemon.data_dir = data_dir.to_string_lossy().into_owned();
    cfg.daemon.log_dir = app_data.join("logs").to_string_lossy().into_owned();
    let token = random_token();
    cfg.daemon.auth_token = token.clone();

    let shutdown = CancellationToken::new();
    let alert_webhook = cfg.daemon.alert_webhook.clone();
    let state = build_state(cfg, None);
    let supervisor = state.supervisor.clone();
    let router = build_router(state);

    // block_on:setup 是同步上下文,需在返回前拿到实际端口
    let listener =
        tauri::async_runtime::block_on(async { TcpListener::bind("127.0.0.1:0").await })?;
    let port = listener.local_addr()?.port();

    // 编排对齐 lib.rs run_app_with_shutdown:start_auto → desired → metrics → health → serve
    let boot_sup = supervisor.clone();
    let serve_shutdown = shutdown.clone();
    let done = tauri::async_runtime::spawn(async move {
        boot_sup.start_auto().await;
        boot_sup.start_desired().await;
        boot_sup.clone().spawn_metrics(Duration::from_secs(2));
        warden::supervisor::health::spawn_health(boot_sup.clone(), alert_webhook);

        // shutdown 触发时逆序停全部被监护服务(graceful),与 serve 收尾并行
        let stop_sup = boot_sup.clone();
        let stop_shutdown = serve_shutdown.clone();
        let drain_limit = stop_shutdown.clone();
        let stop_task = tokio::spawn(async move {
            stop_shutdown.cancelled().await;
            stop_sup.stop_all().await;
        });
        let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
            serve_shutdown.cancelled().await;
        });
        tokio::select! {
            r = serve => {
                if let Err(e) = r {
                    eprintln!("[warden-desktop] axum serve error: {e}");
                }
            }
            _ = async move {
                drain_limit.cancelled().await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            } => {
                eprintln!("[warden-desktop] HTTP 连接 5s 内未全部关闭,强制断开");
            }
        }
        let _ = stop_task.await;
    });

    Ok(EmbeddedDaemon {
        port,
        token,
        supervisor,
        shutdown,
        done: tokio::sync::Mutex::new(Some(done)),
    })
}

/// 桌面端内嵌 daemon 句柄。
pub struct EmbeddedDaemon {
    pub port: u16,
    pub token: String,
    /// 监护引擎句柄(预留:P2 托盘快捷操作直接进程内调用)。
    #[expect(dead_code)]
    pub supervisor: std::sync::Arc<warden::supervisor::Supervisor>,
    shutdown: CancellationToken,
    done: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl EmbeddedDaemon {
    /// 停止:触发 shutdown → stop_all(逆序优雅)→ serve 收尾。幂等。
    pub async fn stop(&self) {
        self.shutdown.cancel();
        if let Some(t) = self.done.lock().await.take() {
            let _ = t.await;
        }
    }
}

/// 生成 32 字符 hex 随机 token(getrandom;仅 127.0.0.1 监听的进程内凭据)。
fn random_token() -> String {
    let mut buf = [0u8; 16];
    let _ = getrandom::fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
