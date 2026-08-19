//! 单进程监护:spawn 子进程、接管输出、监听退出、按策略重启。
//!
//! `supervise` 是一个循环:启动 → 等待退出(或被 cancel)→ 按
//! `auto_restart` 与 `RestartPolicy` 决策重启/熔断,直到主动停止或 Failed。

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::logs::{LogHub, LogStream};
use crate::model::{ProcState, RestartMode};

use super::ProcHandle;

/// 监护循环:启动 → 监听退出 → 按策略重启,直到主动停止或熔断。
/// spawn 失败(命令不存在/无权限等)是**零重试**:直接 Failed 不进重启决策
/// ——区别于进程退出后的 backoff 熔断路径(坏路径重试无意义且徒刷日志)。
pub async fn supervise(handle: Arc<ProcHandle>, cancel: CancellationToken) {
    loop {
        // 每次重启取最新配置快照(运行中 update 保存的配置在此生效)
        let cfg = handle.inner.lock().unwrap().config.clone();
        let mut child = match spawn_child(&cfg) {
            Ok(c) => c,
            Err(e) => {
                let mut g = handle.inner.lock().unwrap();
                g.state = ProcState::Failed {
                    reason: format!("spawn 失败:{e}"),
                    exit_code: None,
                    at: Utc::now(),
                };
                handle.log.push(
                    LogStream::Stderr,
                    crate::logs::LEVEL_ERROR,
                    format!("[warden] spawn 失败:{e}"),
                );
                // 同步落文件日志:LogHub 只在 UI/日志 API 可见,warden.log 无痕迹
                // 会让现场排查(命令不存在/目录无效等)完全无线索。
                tracing::warn!(
                    "[warden] 服务 '{}' spawn 失败:{e}(command={}, working_dir={:?})",
                    cfg.name,
                    cfg.command,
                    cfg.working_dir
                );
                return;
            }
        };
        let pid = child.id();
        let pgid = pid.unwrap_or(0);

        // 接管输出:两个 reader task 把 stdout/stderr 按行解码后推入 LogHub。
        // 解码编码按 output_encoding 解析(默认 UTF-8),GBK 等中文编码不再丢行。
        let encoding = resolve_encoding(&cfg.output_encoding);
        if let Some(out) = child.stdout.take() {
            let log = Arc::clone(&handle.log);
            tokio::spawn(pipe_reader(out, log, LogStream::Stdout, encoding));
        }
        if let Some(err) = child.stderr.take() {
            let log = Arc::clone(&handle.log);
            tokio::spawn(pipe_reader(err, log, LogStream::Stderr, encoding));
        }

        // 进程树追踪:创建 Job Object / 进程组,assign 子进程,存入 ProcInner
        //   stop 时强杀整棵树(含孤儿)+ warden 崩溃时 KILL_ON_JOB_CLOSE 保护。
        //   存 Mutex<ProcInner> 而非 loop 局部,避免 &JobGuard 跨 await 导致 future !Send。
        {
            let job = match super::signal::create_job_tree() {
                Ok(j) => {
                    if let Err(e) = super::signal::assign_to_job(&j, &child) {
                        tracing::warn!("[supervisor] assign job 失败({}):{e}", cfg.name);
                    }
                    Some(j)
                }
                Err(e) => {
                    tracing::warn!("[supervisor] create job 失败({}):{e}", cfg.name);
                    None
                }
            };
            handle.inner.lock().unwrap().job = job;
        }

        // 设 Running,并在 restart_window 外重置计数(稳定运行后重新给机会)
        let now = Utc::now();
        {
            let mut g = handle.inner.lock().unwrap();
            if let Some(last) = g.last_started_at {
                let elapsed = (now - last).num_seconds().max(0) as u64;
                if elapsed >= cfg.restart.restart_window_secs {
                    g.restart_count = 0;
                }
            }
            g.last_started_at = Some(now);
            g.state = ProcState::Running {
                pid: pgid,
                started_at: now,
            };
        }
        handle.log.push(
            LogStream::Stdout,
            crate::logs::LEVEL_INFO,
            format!("[warden] 进程启动 pid={pid:?}"),
        );

        // 等待退出或主动停止
        let exit_code = tokio::select! {
            status = child.wait() => match status {
                Ok(s) => s.code(),
                Err(_) => None,
            },
            _ = cancel.cancelled() => {
                // 优雅停止:发信号 → 等 graceful_timeout → 超时强杀整棵树
                {
                    let mut g = handle.inner.lock().unwrap();
                    g.state = ProcState::Stopping;
                }
                handle.log.push(LogStream::Stdout, crate::logs::LEVEL_INFO, "[warden] 发送优雅停止信号");
                if let Err(e) = super::signal::send_graceful(pgid) {
                    tracing::warn!("[supervisor] 发送 graceful 信号失败({}):{e}", cfg.name);
                }
                let timeout = std::time::Duration::from_secs(cfg.graceful_timeout_secs);
                match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(_) => {
                        handle.log.push(LogStream::Stdout, crate::logs::LEVEL_INFO, "[warden] 优雅停止完成");
                    }
                    Err(_) => {
                        handle.log.push(LogStream::Stderr, crate::logs::LEVEL_WARN, "[warden] 优雅停止超时,强杀进程树");
                        {
                            let g = handle.inner.lock().unwrap();
                            if let Some(j) = &g.job {
                                super::signal::force_kill_tree(j, pgid);
                            }
                        }
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                    }
                }
                let mut g = handle.inner.lock().unwrap();
                g.state = ProcState::Stopped;
                handle.log.push(LogStream::Stdout, crate::logs::LEVEL_INFO, "[warden] 已停止");
                return;
            }
        };

        // race 保护:wait 完成与 cancel 同时发生时按主动停止处理
        if cancel.is_cancelled() {
            let mut g = handle.inner.lock().unwrap();
            g.state = ProcState::Stopped;
            return;
        }

        handle.log.push(
            LogStream::Stderr,
            crate::logs::LEVEL_INFO,
            format!("[warden] 进程退出 code={exit_code:?}"),
        );

        // 记录最近一次自然退出(崩溃/正常退出;主动 stop 不经此路径),供 TUI/排查
        {
            let mut g = handle.inner.lock().unwrap();
            g.last_exit = Some(crate::model::LastExit {
                exit_code,
                at: Utc::now(),
            });
        }

        // 实际生效的退出模式:auto_restart=false 强制退化为 Never(避免双重表达);
        // 否则沿用配置 mode。该值在退出码已知后,统一用于下方决策分支。
        let effective_mode = if cfg.auto_restart {
            cfg.restart.mode.clone()
        } else {
            RestartMode::Never
        };

        // 未启用自动重启 → Failed(历史行为保持)
        if matches!(effective_mode, RestartMode::Never) {
            let mut g = handle.inner.lock().unwrap();
            g.state = ProcState::Failed {
                reason: "进程退出且未启用 auto_restart".into(),
                exit_code,
                at: Utc::now(),
            };
            return;
        }

        // Unexpected 模式:退出码在白名单内 → 视作预期退出,状态 Stopped,
        // 不重启。last_exit 已在外部记录;restart_count 不增不减
        // (自然衰减由 restart_window_secs 决定)。
        // 注意:exit_code 为 None 时(信号杀掉),-1 哨兵不命中白名单,
        // 退化为 backoff 路径,与 supervisord 同款限制——子进程被信号杀
        // 无法区分"主动退出"与"被 SIGKILL",保守起见走 backoff。
        if matches!(effective_mode, RestartMode::Unexpected) {
            let exit = exit_code.unwrap_or(-1);
            if cfg.restart.expected_exit_codes.contains(&exit) {
                let mut g = handle.inner.lock().unwrap();
                g.state = ProcState::Stopped;
                handle.log.push(
                    LogStream::Stdout,
                    crate::logs::LEVEL_INFO,
                    format!("[warden] 预期退出 code={exit},不重启(matched expected_exit_codes)"),
                );
                return;
            }
        }

        // 重启决策:超 max_retries 熔断,否则退避后重试
        let (attempt, delay) = {
            let mut g = handle.inner.lock().unwrap();
            if g.restart_count >= cfg.restart.max_retries {
                g.state = ProcState::Failed {
                    reason: format!("重启次数超限({})", g.restart_count),
                    exit_code,
                    at: Utc::now(),
                };
                handle.log.push(
                    LogStream::Stderr,
                    crate::logs::LEVEL_ERROR,
                    "[warden] 重启次数超限,进入 Failed",
                );
                return;
            }
            g.restart_count += 1;
            let attempt = g.restart_count;
            let delay = cfg.restart.backoff_ms(attempt);
            let next_at = Utc::now() + chrono::Duration::milliseconds(delay as i64);
            g.state = ProcState::Restarting { attempt, next_at };
            (attempt, delay)
        };
        handle.log.push(
            LogStream::Stdout,
            crate::logs::LEVEL_INFO,
            format!("[warden] {delay}ms 后第 {attempt} 次重启"),
        );

        // backoff 期间仍响应主动停止
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            _ = cancel.cancelled() => {
                let mut g = handle.inner.lock().unwrap();
                g.state = ProcState::Stopped;
                return;
            }
        }
        // 回到 loop 顶部重新 spawn
    }
}

fn spawn_child(config: &crate::model::ServiceConfig) -> std::io::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(&config.command);
    cmd.args(&config.args);
    if let Some(wd) = &config.working_dir {
        cmd.current_dir(wd);
    }
    for (k, v) in &config.environment {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    super::signal::prepare_command(&mut cmd);
    cmd.spawn()
}

/// 把子进程输出流按行解码后推入 LogHub,流结束(EOF)时自然退出。
///
/// 用 `read_until(b'\n')` 读原始字节行,再按 `encoding` 解码——而非
/// `BufReader::lines()`(它强制 UTF-8,遇非法字节报错导致整行静默丢失)。
/// 行切分安全:GBK 双字节尾字节、UTF-8 continuation 均不含 0x0A(不支持 UTF-16)。
/// 非法字节以 U+FFFD 替换(而非丢整行),与原行为相比对坏数据更友好。
async fn pipe_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    stream: R,
    log: Arc<LogHub>,
    kind: LogStream,
    encoding: &'static encoding_rs::Encoding,
) {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(stream);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                // 去掉行尾换行符(\n 及前导 \r),与原 lines() 行为一致
                let mut end = buf.len();
                while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
                    end -= 1;
                }
                let (text, _, _) = encoding.decode(&buf[..end]);
                if !text.is_empty() {
                    // 剥离 ANSI 转义序列(被监护程序常输出彩色日志;LogHub 按纯文本
                    // 存储/展示,ANSI 码会显示为乱码)。tracing 风格的 [32m 等即属此类。
                    let text = strip_ansi_escapes::strip_str(&text);
                    if !text.is_empty() {
                        let level = detect_level(&text);
                        log.push(kind, level, text);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// 启发式识别日志等级(被监护进程是黑盒,只能按常见格式模式匹配)。
/// 优先级 error > warn > info > debug;未匹配返回 "unknown"。
/// 识别规则(大小写不敏感):`[level]`、`level:`、行首 `level `、独立词 ` level `
/// (覆盖 Rust tracing 的 `... INFO target: msg` 与 log4j/`[ERROR]` 风格)。
/// 启发式有误报可能(如正文出现 "error:"),文档明示,UI 提供人工筛选。
fn detect_level(text: &str) -> &'static str {
    let lower = text.to_ascii_lowercase();
    for (tok, lv) in [
        ("error", crate::logs::LEVEL_ERROR),
        ("warn", crate::logs::LEVEL_WARN),
        ("info", crate::logs::LEVEL_INFO),
        ("debug", "debug"),
    ] {
        let open = format!("[{tok}]");
        let colon = format!("{tok}:");
        let sp = format!(" {tok} ");
        if lower.contains(&open)
            || lower.contains(&colon)
            || lower.starts_with(&format!("{tok} "))
            || lower.contains(&sp)
        {
            return lv;
        }
    }
    "unknown"
}

/// 解析配置的编码标签为 encoding_rs 编码;None/空/未识别均回退 UTF-8。
fn resolve_encoding(name: &Option<String>) -> &'static encoding_rs::Encoding {
    match name.as_deref() {
        None | Some("") => encoding_rs::UTF_8,
        Some(label) => encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or_else(|| {
            tracing::warn!("[supervisor] 未知输出编码 '{label}',回退 UTF-8");
            encoding_rs::UTF_8
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::{LogHub, LogStream};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    /// 验证意图:中文 Windows 控制台程序输出 GBK,warden 必须正确解码而非丢行。
    #[tokio::test]
    async fn pipe_reader_decodes_gbk_output() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let log = Arc::new(LogHub::new(None));
        // "你好,warden" 的 GBK 编码 + CRLF 行尾
        let (line1, _, _) = encoding_rs::GBK.encode("你好,warden\r\n");
        tx.write_all(&line1).await.unwrap();
        // 第二行验证按 \n 切分(GBK 双字节不含 0x0A,切分安全)
        let (line2, _, _) = encoding_rs::GBK.encode("再见\n");
        tx.write_all(&line2).await.unwrap();
        drop(tx); // 关闭发送端触发 EOF

        pipe_reader(rx, Arc::clone(&log), LogStream::Stdout, encoding_rs::GBK).await;

        let snap = log.snapshot(10);
        assert_eq!(snap.len(), 2, "应为 2 行,实际 {snap:?}");
        assert!(snap[0].text.contains("你好"), "第1行解码:{}", snap[0].text);
        assert!(!snap[0].text.contains('\n'), "行尾换行应被去除");
        assert!(!snap[0].text.contains('\r'), "行尾回车应被去除");
        assert!(snap[1].text.contains("再见"), "第2行解码:{}", snap[1].text);
    }

    /// 验证意图:默认 UTF-8 解码不被破坏(改动回归保护)。
    #[tokio::test]
    async fn pipe_reader_preserves_utf8() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let log = Arc::new(LogHub::new(None));
        tx.write_all("第一行\nsecond line\n".as_bytes())
            .await
            .unwrap();
        drop(tx);

        pipe_reader(rx, Arc::clone(&log), LogStream::Stdout, encoding_rs::UTF_8).await;

        let snap = log.snapshot(10);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].text, "第一行");
        assert_eq!(snap[1].text, "second line");
    }

    /// 验证意图:被监护程序(如 rs-iot 的 tracing 输出)带 ANSI 颜色码,
    /// LogHub 应存纯文本——[32m 等转义码不能当乱码显示;纯 ANSI 行整行丢弃。
    #[tokio::test]
    async fn pipe_reader_strips_ansi_codes() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let log = Arc::new(LogHub::new(None));
        // 模拟 tracing 彩色输出:ESC[32m(绿) INFO ESC[0m(复位) 消息
        tx.write_all(b"\x1b[32mINFO\x1b[0m starting\n")
            .await
            .unwrap();
        // 纯 ANSI 样式行(如仅暗淡+复位)应整行丢弃,不留空行
        tx.write_all(b"\x1b[2m\x1b[0m\n").await.unwrap();
        drop(tx);

        pipe_reader(rx, Arc::clone(&log), LogStream::Stdout, encoding_rs::UTF_8).await;

        let snap = log.snapshot(10);
        assert_eq!(snap.len(), 1, "纯 ANSI 行应被丢弃:{snap:?}");
        assert_eq!(snap[0].text, "INFO starting");
        assert!(!snap[0].text.contains('\u{1b}'), "不应残留转义字符");
    }

    /// 验证意图:等级识别覆盖常见日志格式(括号/冒号/行首/独立词/tracing 风格)。
    #[test]
    fn detect_level_recognizes_common_formats() {
        assert_eq!(detect_level("[ERROR] connect failed"), "error");
        assert_eq!(detect_level("error: timeout"), "error");
        assert_eq!(detect_level("ERROR connect failed"), "error");
        assert_eq!(
            detect_level("2026-08-14T10:00:00Z ERROR rs_iot::db: lux SAVE failed"),
            "error"
        );
        assert_eq!(detect_level("WARN: retry"), "warn");
        assert_eq!(detect_level("2026-08-14T10:00:00Z  WARN target: x"), "warn");
        assert_eq!(detect_level("INFO started"), "info");
        assert_eq!(detect_level("[info] listening"), "info");
        assert_eq!(detect_level("DEBUG detail"), "debug");
        assert_eq!(detect_level("hello world"), "unknown");
        // 大写不敏感 + 优先级 error > warn
        assert_eq!(detect_level("[WARN] [ERROR] both"), "error");
        assert_eq!(detect_level("plain text no marker"), "unknown");
    }
}
