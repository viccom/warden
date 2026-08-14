//! OS 服务注册:Windows Service / systemd unit。

#[cfg(windows)]
pub mod windows;
#[cfg(unix)]
pub mod systemd;

/// 安装为 OS 服务(Windows: `sc create` / Linux: 写 systemd unit)。需管理员/root。
pub fn install() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    println!("binary: {}", exe.display());
    #[cfg(windows)]
    {
        windows::install(&exe)?;
    }
    #[cfg(unix)]
    {
        systemd::install(&exe)?;
    }
    Ok(())
}

/// 卸载 OS 服务。
pub fn uninstall() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows::uninstall()?;
    }
    #[cfg(unix)]
    {
        systemd::uninstall()?;
    }
    Ok(())
}
