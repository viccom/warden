//! warden —— 进程监护管理工具(binary 入口)。
//!
//! CLI 由 clap derive 定义;库逻辑在 `warden` crate(`src/lib.rs`)。
//! 设计见 docs/DESIGN.md,进度见 docs/ROADMAP.md。

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "warden",
    version,
    about = "进程监护管理工具(supervisor daemon)"
)]
struct Cli {
    /// 配置文件路径(toml)。未指定则按路径查找规则定位。
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 前台运行 daemon(监护引擎 + HTTP API)。Phase 1 主入口。
    Run,
    /// 启动 TUI 客户端(连 HTTP API)。— Phase 3 实现。
    Tui,
    /// 把 warden 自身注册成 OS 服务(开机自启)。— Phase 2 实现。
    Install,
    /// 卸载 OS 服务注册。— Phase 2 实现。
    Uninstall,
    /// 由 OS 服务管理器调用的事件循环入口。— Phase 2 实现。
    Service,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => warden::run_app(cli.config).await,
        Command::Tui => bail_unimplemented("tui", "Phase 3"),
        Command::Install => {
            warden::service::install()?;
            Ok(())
        }
        Command::Uninstall => {
            warden::service::uninstall()?;
            Ok(())
        }
        Command::Service => {
            #[cfg(windows)]
            {
                warden::service::windows::dispatch()?;
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("`warden service` 仅 Windows 支持");
            }
            Ok(())
        }
    }
}

/// Phase 1 尚未实现的子命令统一返回友好提示。
fn bail_unimplemented(cmd: &str, phase: &str) -> anyhow::Result<()> {
    anyhow::bail!("`warden {cmd}` 将在 {phase} 实现,当前仅完成 Phase 1(见 docs/ROADMAP.md)")
}
