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

        // 接管输出:两个 reader task 把 stdout/stderr 按行推入 LogHub
        if let Some(out) = child.stdout.take() {
            let log = Arc::clone(&handle.log);
            tokio::spawn(pipe_reader(out, log, LogStream::Stdout));
        }
        if let Some(err) = child.stderr.take() {
            let log = Arc::clone(&handle.log);
            tokio::spawn(pipe_reader(err, log, LogStream::Stderr));
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

        handle
            .log
            .push(LogStream::Stderr, format!("[warden] 进程退出 code={exit_code:?}"));

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
        handle
            .log
            .push(LogStream::Stdout, format!("[warden] {delay}ms 后第 {attempt} 次重启"));

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

/// 把子进程输出流按行推入 LogHub,流结束(EOF)时自然退出。
async fn pipe_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    stream: R,
    log: Arc<LogHub>,
    kind: LogStream,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log.push(kind, line);
    }
}
