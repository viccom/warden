//! 测试辅助程序(由 warden 作为被监护进程启动),验证优雅停止信号链路。
//!
//! 用法:`graceful_target <marker_path> [--stubborn] [--child]`
//! - 收到 console 信号(CTRL_C / CTRL_BREAK)后写标记文件(证明信号投递成功)
//! - 默认收到即 exit 0(graceful)
//! - `--stubborn`:收到后继续运行,等 warden 超时强杀(测 force_kill)
//! - `--child`:额外 spawn 一个长期子进程(测 Job Object 杀整棵进程树)

#[cfg(windows)]
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

#[cfg(windows)]
mod handler {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// 收到 CTRL_C(0)或 CTRL_BREAK(1)后置位。
    pub static GOT: AtomicBool = AtomicBool::new(false);
    pub extern "system" fn on_event(ctrl: u32) -> i32 {
        if ctrl == 0 || ctrl == 1 {
            GOT.store(true, Ordering::SeqCst);
            1 // TRUE:已处理,阻止默认 handler
        } else {
            0
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let marker = args.get(1).cloned().unwrap_or_default();
    let stubborn = args.iter().any(|a| a == "--stubborn");
    let spawn_child = args.iter().any(|a| a == "--child");

    if spawn_child {
        let mut cmd = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            cmd.args(["-n", "120", "127.0.0.1"]);
        } else {
            cmd.arg("120");
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = cmd.spawn();
    }

    // 安装 console handler(Windows)或 await ctrl_c(Unix)
    #[cfg(windows)]
    {
        unsafe {
            SetConsoleCtrlHandler(Some(handler::on_event), 1);
        }
        // handler 就绪标记:测试等它落盘后再发停止信号,消除
        // "CTRL_BREAK 先于 handler 安装到达 → 默认 handler 直接杀进程"的竞态
        if !marker.is_empty() {
            let _ = std::fs::write(format!("{marker}.ready"), "");
        }
        while !handler::GOT.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }

    // 收到信号:写标记
    if !marker.is_empty() {
        let _ = std::fs::write(&marker, "graceful\n");
    }
    if !stubborn {
        std::process::exit(0);
    }
    // stubborn:继续 sleep,等 warden 超时后 force_kill 整棵树
    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
}
