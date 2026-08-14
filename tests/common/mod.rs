//! 测试公共工具:跨平台无害的被监护命令。

/// 长期运行的命令(约 60s),用于测试 Running/stop。
///
/// 直接调用 ping/sleep 而非 `cmd /c ping`:否则 stop 时 kill 的是 cmd,
/// ping 作为孙子进程残留为孤儿(Phase 4 将用 Windows Job Object 杀整棵进程树解决)。
pub fn long_runner() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "ping".into(),
            vec!["-n".into(), "60".into(), "127.0.0.1".into()],
        )
    } else {
        ("sleep".into(), vec!["60".into()])
    }
}

/// 立即以非零码退出的命令,用于测试退出/重启/熔断。
#[allow(dead_code)] // 跨测试共享 helper,部分 target 不使用
pub fn quick_fail() -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd".into(), vec!["/c".into(), "exit".into(), "1".into()])
    } else {
        ("false".into(), vec![])
    }
}
