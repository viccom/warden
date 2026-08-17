//! 内嵌 daemon:桌面版进程内跑 warden 监护引擎 + HTTP API。
//!
//! 与 CLI `run` 的差异:
//! - 绑 `127.0.0.1:0` 随机端口(避免与用户手跑的 CLI warden 8789 冲突),
//!   实际端口 + 随机 token 经 Tauri 命令交给前端
//! - data_dir/log_dir 重定向到桌面应用数据目录(与 CLI 隔离:
//!   两版各自的配置文件/数据/日志互不干扰)
//! - 默认配置文件名独立(`services.desktop.toml`,与 CLI 的 `services.toml`
//!   区分;查找链不落 cwd 与 CLI 平台位置,防止拾取 CLI 配置)
//! - 无 console 信号处理(退出由托盘/命令触发 shutdown token)
//! - 配置缺失不退出(空配置起步,服务经 CRUD 添加——写回时创建配置文件)
//!
//! serve 编排(start_auto→desired→metrics→health→serve→stop_all)复用
//! `warden::serve_with_shutdown`,与 CLI/Service 同一实现。

use std::path::{Path, PathBuf};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// 桌面版默认配置文件名(与 CLI 的 services.toml 区分,避免互拾对方配置)。
const DESKTOP_CONFIG_FILE: &str = "services.desktop.toml";

/// 解析桌面版配置路径(纯函数便于测试):
/// 1. `$WARDEN_CONFIG`(显式指定;不存在则跳过)
/// 2. `<exe_dir>/config/services.desktop.toml`(绿色部署:配置随程序走)
/// 3. `<app_data>/config/services.desktop.toml`(标准可写位置,找不到时的默认)
///
/// 不回落到 cwd / CLI 平台位置——桌面版 cwd 随启动方式漂移,且不应拾取
/// CLI 的 services.toml。
fn resolve_config_path(
    env_cfg: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    app_data: &Path,
) -> PathBuf {
    if let Some(p) = env_cfg {
        if p.exists() {
            return p;
        }
    }
    if let Some(dir) = exe_dir {
        let p = dir.join("config").join(DESKTOP_CONFIG_FILE);
        if p.exists() {
            return p;
        }
    }
    app_data.join("config").join(DESKTOP_CONFIG_FILE)
}

/// 起内嵌 daemon(阻塞直到 listener 就绪),返回端口/token 与停止句柄。
pub fn start(app_data: &Path) -> anyhow::Result<EmbeddedDaemon> {
    // Windows:确保持有隐藏 console → 被监护子进程不弹终端窗口,
    // 且 CTRL_BREAK 优雅停止可投递(无 console 宿主二者皆失)。
    #[cfg(windows)]
    warden::supervisor::signal::ensure_hidden_console();

    // 日志先行:文件层落 <app_data>/logs(GUI 无 console,eprintln 不可见;
    // 内嵌 daemon 此前无任何日志,现场问题无法回溯——API 请求经 TraceLayer
    // 也会进此文件,CRUD/鉴权失败可查)
    let log_dir = app_data.join("logs").to_string_lossy().into_owned();
    let log_guard = warden::init_tracing(&log_dir);

    // 配置:$WARDEN_CONFIG 显式优先;默认查找名独立于 CLI(services.desktop.toml);
    // 缺配置则空集起步(服务可经界面 CRUD 添加)
    let env_cfg = std::env::var("WARDEN_CONFIG").ok().map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let cfg_path = resolve_config_path(env_cfg, exe_dir, app_data);
    let mut cfg = if cfg_path.exists() {
        tracing::info!("[warden-desktop] 加载配置 {}", cfg_path.display());
        warden::config::Config::load(Some(&cfg_path)).unwrap_or_else(|e| {
            tracing::warn!(
                "[warden-desktop] 配置加载失败({e}),以空配置启动(可经界面 CRUD 添加服务)"
            );
            warden::config::Config::default()
        })
    } else {
        tracing::warn!(
            "[warden-desktop] 未找到配置文件(默认路径 {}),以空配置启动(可经界面 CRUD 添加服务)",
            cfg_path.display()
        );
        warden::config::Config::default()
    };
    // 桌面隔离:数据/日志独立目录,鉴权用进程内随机 token
    let data_dir = app_data.join("data");
    std::fs::create_dir_all(&data_dir)?;
    cfg.daemon.data_dir = data_dir.to_string_lossy().into_owned();
    cfg.daemon.log_dir = log_dir;
    let token = random_token();
    cfg.daemon.auth_token = token.clone();

    let shutdown = CancellationToken::new();
    // block_on:setup 是同步上下文,需在返回前拿到实际端口
    let listener =
        tauri::async_runtime::block_on(async { TcpListener::bind("127.0.0.1:0").await })?;
    let port = listener.local_addr()?.port();

    let serve_shutdown = shutdown.clone();
    let done = tauri::async_runtime::spawn(async move {
        // config_path 传解析出的桌面路径:API reload 重读同一文件,
        // 不再回落 CLI 查找链(避免 reload 拾取 CLI 的 services.toml)
        if let Err(e) =
            warden::serve_with_shutdown(cfg, Some(cfg_path), listener, serve_shutdown).await
        {
            tracing::error!("[warden-desktop] 内嵌 daemon 退出:{e}");
        }
    });

    Ok(EmbeddedDaemon {
        port,
        token,
        shutdown,
        done: tokio::sync::Mutex::new(Some(done)),
        log_guard,
    })
}

/// 桌面端内嵌 daemon 句柄。
pub struct EmbeddedDaemon {
    pub port: u16,
    pub token: String,
    shutdown: CancellationToken,
    done: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// tracing 文件层 guard(RAII:持到进程结束,drop 会丢尾部日志;永不显式读)。
    #[expect(dead_code)]
    log_guard: warden::LogGuard,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
        p.to_path_buf()
    }

    /// 意图:桌面版默认配置解析必须与 CLI 隔离——仅认 $WARDEN_CONFIG /
    /// exe_dir / app_data 三处,默认名固定 services.desktop.toml,
    /// 绝不落到 cwd 或 CLI 的平台位置(services.toml)。
    #[test]
    fn resolve_config_path_env_then_exe_then_app_data() {
        let tmp = std::env::temp_dir().join(format!("warden-desktop-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let app_data = tmp.join("appdata");

        // 全缺 → app_data 默认路径
        let p = resolve_config_path(None, None, &app_data);
        assert_eq!(p, app_data.join("config").join(DESKTOP_CONFIG_FILE));

        // exe_dir 有 → 胜出
        let exe_cfg = touch(&tmp.join("exe").join("config").join(DESKTOP_CONFIG_FILE));
        assert_eq!(
            resolve_config_path(None, Some(tmp.join("exe")), &app_data),
            exe_cfg
        );

        // $WARDEN_CONFIG 存在 → 最高优先
        let env_cfg = touch(&tmp.join("env").join("my.toml"));
        assert_eq!(
            resolve_config_path(Some(env_cfg.clone()), Some(tmp.join("exe")), &app_data),
            env_cfg
        );

        // $WARDEN_CONFIG 指了不存在文件 → 跳过,落 exe_dir
        assert_eq!(
            resolve_config_path(
                Some(tmp.join("env").join("missing.toml")),
                Some(tmp.join("exe")),
                &app_data
            ),
            exe_cfg
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
