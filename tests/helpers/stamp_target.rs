//! 测试辅助程序(由 warden 作为被监护进程启动),验证有序启停(分组/优先级)。
//!
//! 用法:`stamp_target <name> <stamp_file>`
//! - 启动即向 stamp_file **追加**一行 `start:<name>`(多进程追加同一文件,行序即启动序)
//! - 收到停止信号(Windows CTRL_C/CTRL_BREAK,Unix SIGTERM——与 warden 生产信号路径一致)
//!   后追加 `stop:<name>` 再 exit 0

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

/// 以追加模式写一行标记(多进程写同一文件,O_APPEND 小写入原子性足够)。
fn stamp(path: &str, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).cloned().unwrap_or_default();
    let stamp_file = args.get(2).cloned().unwrap_or_default();

    // 先写启动标记,再等停止信号
    if !stamp_file.is_empty() {
        stamp(&stamp_file, &format!("start:{name}"));
    }

    #[cfg(windows)]
    {
        unsafe {
            SetConsoleCtrlHandler(Some(handler::on_event), 1);
        }
        while !handler::GOT.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    #[cfg(not(windows))]
    {
        // warden stop 在 Unix 发 SIGTERM(与生产信号路径一致,对齐 graceful_target 的差异修正)
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 监听");
        term.recv().await;
    }

    if !stamp_file.is_empty() {
        stamp(&stamp_file, &format!("stop:{name}"));
    }
    std::process::exit(0);
}
