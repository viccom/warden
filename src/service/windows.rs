//! Windows Service:SCM dispatch + install/uninstall。
//!
//! 照 rs-iot `src/service/windows.rs` 范式:`define_windows_service!` +
//! `service_dispatcher` + `service_control_handler`。service_main 在会话 0 跑;
//! AllocConsole 创建不可见 console,让被监护子进程继承 → CTRL_BREAK graceful
//! 链路在 Service 模式仍有效。

use std::sync::mpsc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

use crate::config::Config;

pub(crate) const SERVICE_NAME: &str = "warden";

windows_service::define_windows_service!(ffi_service_main, service_main);

/// 进入 SCM dispatch(由 main 的 `service` 子命令调用,阻塞直到服务停止)。
pub fn dispatch() -> anyhow::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("service dispatch failed: {e:?}"))
}

fn service_main(_arguments: Vec<std::ffi::OsString>) {
    // 会话 0 默认无 console:AllocConsole 创建不可见 console,使被监护子进程继承
    // → CTRL_BREAK graceful 链路在 Service 模式下仍有效。
    unsafe {
        let _ = windows_sys::Win32::System::Console::AllocConsole();
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let event_handler = move |ctrl: ServiceControl| -> ServiceControlHandlerResult {
        match ctrl {
            ServiceControl::Stop => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[warden] register control handler failed: {e:?}");
            return;
        }
    };

    report(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(0),
        1,
        Duration::from_secs(10),
    );

    // 加载配置(Service 模式 cwd=System32,靠 find_config_path 的 exe_dir 分支定位)
    let config_path: Option<std::path::PathBuf> = None;
    let cfg = match Config::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[warden] 配置加载失败:{e}");
            report(
                &status_handle,
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                ServiceExitCode::ServiceSpecific(1),
                0,
                Duration::default(),
            );
            return;
        }
    };

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let (done_tx, done_rx) = mpsc::channel::<bool>();

    // worker 线程:独立 tokio runtime 跑 run_app_with_shutdown
    let _app_thread = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[warden] tokio runtime: {e}");
                let _ = done_tx.send(false);
                return;
            }
        };
        let ok = rt
            .block_on(crate::run_app_with_shutdown(cfg, config_path, shutdown_clone))
            .is_ok();
        let _ = done_tx.send(ok);
    });

    report(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP,
        ServiceExitCode::ServiceSpecific(0),
        0,
        Duration::default(),
    );

    // 等 SCM Stop
    let _ = stop_rx.recv();

    report(
        &status_handle,
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(0),
        1,
        Duration::from_secs(30),
    );
    shutdown.cancel();

    // 等 run_app_with_shutdown 返回(stop_all 子进程 graceful 完成)
    let _ = done_rx.recv();

    report(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(0),
        0,
        Duration::default(),
    );

    std::process::exit(0);
}

fn report(
    handle: &windows_service::service_control_handler::ServiceStatusHandle,
    state: ServiceState,
    controls: ServiceControlAccept,
    exit_code: ServiceExitCode,
    checkpoint: u32,
    wait_hint: Duration,
) {
    let status = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: controls,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    };
    if let Err(e) = handle.set_service_status(status) {
        eprintln!("[warden] set_service_status({state:?}) failed: {e:?}");
    }
}

/// install:sc.exe create binPath="<exe> service" start=auto(需管理员)。
pub fn install(exe: &std::path::Path) -> anyhow::Result<()> {
    let binpath = format!("\"{}\" service", exe.display());
    let status = std::process::Command::new("sc")
        .args([
            "create",
            SERVICE_NAME,
            "binPath=",
            &binpath,
            "start=",
            "auto",
            "DisplayName=",
            "warden",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("sc create 失败(需管理员权限运行)");
    }
    let _ = std::process::Command::new("sc")
        .args(["description", SERVICE_NAME, "warden process supervisor daemon"])
        .status();
    println!(
        "已安装服务 {SERVICE_NAME}。启动:sc start {SERVICE_NAME};停止:sc stop {SERVICE_NAME}"
    );
    Ok(())
}

/// uninstall:sc.exe stop + delete。
pub fn uninstall() -> anyhow::Result<()> {
    let _ = std::process::Command::new("sc")
        .args(["stop", SERVICE_NAME])
        .status();
    let status = std::process::Command::new("sc")
        .args(["delete", SERVICE_NAME])
        .status()?;
    if !status.success() {
        anyhow::bail!("sc delete 失败");
    }
    println!("已卸载服务 {SERVICE_NAME}");
    Ok(())
}
