//! 测试辅助程序(由 warden 作为被监护进程启动),验证监听端口发现。
//!
//! 用法:`port_listener_target [--grandchild]`
//! - 绑定 TCP + UDP 各一个 **随机端口**(127.0.0.1:0,遵守"勿固定端口")
//! - 向 stderr(无缓冲,避免管道块缓冲)打印自报行:
//!   `PORT tcp 127.0.0.1 <port>` / `PORT udp 127.0.0.1 <port>`
//! - 收到停止信号后退出(端口释放)
//! - `--grandchild`:再 spawn 一个 `--child` 自实例(孙进程,stdout/stderr 继承
//!   → 自报行汇入同一管道),验证"启动器形态"服务的孙进程端口可见

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
    let spawn_grandchild = args.iter().any(|a| a == "--grandchild");

    if spawn_grandchild {
        let exe = std::env::current_exe().expect("定位自身 exe");
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--child")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        // 孙进程不 wait:本 helper 退出后由 warden 的 Job Object/进程组回收(测试设计使然)
        #[expect(clippy::zombie_processes)]
        let _grand = cmd.spawn().expect("spawn 孙进程");
    }

    // 绑定随机端口并保持存活(绑定即 LISTEN/已绑定;变量持有到 main 结束)
    let _tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("TCP 绑定");
    let _udp = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("UDP 绑定");
    let tport = _tcp.local_addr().unwrap().port();
    let uport = _udp.local_addr().unwrap().port();
    // stderr 无缓冲,管道下也立即送达 warden 的 pipe_reader
    eprintln!("PORT tcp 127.0.0.1 {tport}");
    eprintln!("PORT udp 127.0.0.1 {uport}");

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
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 监听");
        term.recv().await;
    }
    std::process::exit(0);
}
