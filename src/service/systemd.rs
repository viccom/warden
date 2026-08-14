//! Linux systemd unit 生成。
//!
//! Linux 不用单独 `service` 子命令:systemd 直接 SIGTERM 到 `run`,warden 的
//! `run_app` 监听 ctrl_c(SIGINT)——TODO: 补 SIGTERM 监听(systemd stop 发 SIGTERM)。

const UNIT_PATH: &str = "/etc/systemd/system/warden.service";

pub fn install(exe: &std::path::Path) -> anyhow::Result<()> {
    let unit = format!(
        "[Unit]\nDescription=warden process supervisor daemon\nAfter=network.target\n\n\
         [Service]\nType=simple\nExecStart={exe} run\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        exe = exe.display()
    );
    std::fs::write(UNIT_PATH, unit)
        .map_err(|e| anyhow::anyhow!("写 {UNIT_PATH} 失败:{e}(需 root)"))?;
    println!("已写入 {UNIT_PATH}");
    println!("启用并启动: sudo systemctl enable --now warden");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let _ = std::fs::remove_file(UNIT_PATH);
    println!("已删除 {UNIT_PATH}");
    println!("请执行: sudo systemctl disable warden");
    Ok(())
}
