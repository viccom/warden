//! 测试公共工具:跨平台无害的被监护命令。

/// 长期运行的命令(约 60s),用于测试 Running/stop。
///
/// 直接调用 ping/sleep 而非 `cmd /c ping`:否则 stop 时 kill 的是 cmd,
/// ping 作为孙子进程残留为孤儿(Phase 4 将用 Windows Job Object 杀整棵进程树解决)。
#[allow(dead_code)] // 跨测试共享 helper,部分 target 不使用
pub fn long_runner() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "ping".into(),
            vec!["-n".into(), "60".into(), "127.0.0.1".into()],
        )
    } else {
        // 纯 sleep 全程静默 → SSE/log 捕获类用例拿不到子进程 stdout;改为每秒 echo 一行
        (
            "sh".into(),
            vec![
                "-c".into(),
                "i=0; while [ $i -lt 60 ]; do echo tick-$i; i=$((i+1)); sleep 1; done".into(),
            ],
        )
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

/// 立即以指定退出码退出,用于测试退出码白名单(Unexpected 模式)。
///
/// 跨平台实现:
/// - Windows:`cmd /c exit <code>` —— cmd 自身退出码 = `<code>`
/// - Unix:`sh -c 'exit <code>'` —— sh 在 POSIX 一定可得,退出码可靠
///
/// 选用 `sh -c `<不是 `sh -c 'exit <code>;'` `<避免注入风险)
/// 且参数化形式比拼接 shell 命令更干净。
#[allow(dead_code)] // 跨测试共享 helper,部分 target 不使用
pub fn exit_with(code: i32) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd".into(),
            vec!["/c".into(), "exit".into(), code.to_string()],
        )
    } else {
        ("sh".into(), vec!["-c".into(), format!("exit {code}")])
    }
}
