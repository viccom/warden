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
use crate::model::ProcState;

use super::ProcHandle;

/// 监护循环:启动 → 监听退出 → 按策略重启,直到主动停止或熔断。
pub async fn supervise(handle: Arc<ProcHandle>, cancel: CancellationToken) {
    loop {
        let mut child = match spawn_child(&handle.config) {
            Ok(c) => c,
            Err(e) => {
                let mut g = handle.inner.lock().unwrap();
                g.state = ProcState::Failed {
                    reason: format!("spawn 失败:{e}"),
                    exit_code: None,
                    at: Utc::now(),
                };
                handle
                    .log
                    .push(LogStream::Stderr, format!("[warden] spawn 失败:{e}"));
                return;
            }
        };
        let pid = child.id();
        let pgid = pid.unwrap_or(0);

        // 接管输出:两个 reader task 把 stdout/stderr 按行解码后推入 LogHub。
        // 解码编码按 output_encoding 解析(默认 UTF-8),GBK 等中文编码不再丢行。
        let encoding = resolve_encoding(&handle.config.output_encoding);
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
                        tracing::warn!("[supervisor] assign job 失败({}):{e}", handle.config.name);
                    }
                    Some(j)
                }
                Err(e) => {
                    tracing::warn!("[supervisor] create job 失败({}):{e}", handle.config.name);
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
                if elapsed >= handle.config.restart.restart_window_secs {
                    g.restart_count = 0;
                }
            }
            g.last_started_at = Some(now);
            g.state = ProcState::Running {
                pid: pgid,
                started_at: now,
            };
        }
        handle
            .log
            .push(LogStream::Stdout, format!("[warden] 进程启动 pid={pid:?}"));

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
                handle.log.push(LogStream::Stdout, "[warden] 发送优雅停止信号");
                if let Err(e) = super::signal::send_graceful(pgid) {
                    tracing::warn!("[supervisor] 发送 graceful 信号失败({}):{e}", handle.config.name);
                }
                let timeout = std::time::Duration::from_secs(handle.config.graceful_timeout_secs);
                match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(_) => {
                        handle.log.push(LogStream::Stdout, "[warden] 优雅停止完成");
                    }
                    Err(_) => {
                        handle.log.push(LogStream::Stderr, "[warden] 优雅停止超时,强杀进程树");
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
                handle.log.push(LogStream::Stdout, "[warden] 已停止");
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
            format!("[warden] 进程退出 code={exit_code:?}"),
        );

        // 未启用自动重启 → Failed
        if !handle.config.auto_restart {
            let mut g = handle.inner.lock().unwrap();
            g.state = ProcState::Failed {
                reason: "进程退出且未启用 auto_restart".into(),
                exit_code,
                at: Utc::now(),
            };
            return;
        }

        // 重启决策:超 max_retries 熔断,否则退避后重试
        let (attempt, delay) = {
            let mut g = handle.inner.lock().unwrap();
            if g.restart_count >= handle.config.restart.max_retries {
                g.state = ProcState::Failed {
                    reason: format!("重启次数超限({})", g.restart_count),
                    exit_code,
                    at: Utc::now(),
                };
                handle
                    .log
                    .push(LogStream::Stderr, "[warden] 重启次数超限,进入 Failed");
                return;
            }
            g.restart_count += 1;
            let attempt = g.restart_count;
            let delay = handle.config.restart.backoff_ms(attempt);
            let next_at = Utc::now() + chrono::Duration::milliseconds(delay as i64);
            g.state = ProcState::Restarting { attempt, next_at };
            (attempt, delay)
        };
        handle.log.push(
            LogStream::Stdout,
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
                    log.push(kind, text.into_owned());
                }
            }
            Err(_) => break,
        }
    }
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
}
