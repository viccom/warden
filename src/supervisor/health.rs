//! 健康检查:周期探测 Running 服务的 TCP 端点,维护 HealthStatus 并在状态迁移时告警。
//!
//! 告警三路:tracing warn + LogHub 推送(服务日志)+ 可选 webhook POST(daemon
//! alert_webhook 配置,fire-and-forget)。各服务按自身 interval_secs 到期才检查
//! (外层 1s tick 仅做调度)。

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::model::{HealthCheck, HealthStatus};

use super::Supervisor;

/// 启动健康检查后台 task(daemon 启动时调用一次)。
pub fn spawn_health(
    supervisor: Arc<Supervisor>,
    webhook: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // 先收集到期的检查项(DashMap guard 不跨 await;collect 后逐个探测)
            let due: Vec<_> = supervisor
                .handles
                .iter()
                .filter_map(|entry| {
                    let g = entry.inner.lock().unwrap();
                    let hc = g.config.health.clone()?;
                    if !g.state.is_running() {
                        return None;
                    }
                    let due = match g.health.last_check {
                        Some(last) => {
                            Utc::now() - last >= chrono::Duration::seconds(hc_interval(&hc) as i64)
                        }
                        None => true,
                    };
                    due.then(|| (Arc::clone(entry.value()), hc))
                })
                .collect();
            for (handle, hc) in due {
                check_one(&handle, &hc, webhook.clone()).await;
            }
        }
    })
}

/// HealthCheck 目前只有 Tcp 变体;取 interval_secs。
fn hc_interval(hc: &HealthCheck) -> u64 {
    match hc {
        HealthCheck::Tcp { interval_secs, .. } => *interval_secs,
    }
}

/// 探测单个服务并更新 HealthStatus;状态迁移时告警。
async fn check_one(entry: &super::ProcHandle, hc: &HealthCheck, webhook: Option<String>) {
    let (host, port, timeout_ms) = match hc {
        HealthCheck::Tcp {
            host,
            port,
            timeout_ms,
            ..
        } => (host.clone(), *port, *timeout_ms),
    };
    let attempt = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await;
    let (ok, err) = match attempt {
        Ok(Ok(_)) => (true, None),
        Ok(Err(e)) => (false, Some(format!("connect {host}:{port} 失败:{e}"))),
        Err(_) => (
            false,
            Some(format!("connect {host}:{port} 超时({timeout_ms}ms)")),
        ),
    };

    let (prev, next) = {
        let mut g = entry.inner.lock().unwrap();
        let prev = g.health.status.clone();
        let failures = if ok {
            0
        } else {
            g.health.consecutive_failures + 1
        };
        g.health = HealthStatus {
            status: if ok {
                "healthy".into()
            } else {
                "unhealthy".into()
            },
            last_check: Some(chrono::Utc::now()),
            last_error: err.clone(),
            consecutive_failures: failures,
        };
        (prev, g.health.status.clone())
    };

    if prev != next {
        let name = &entry.inner.lock().unwrap().config.name;
        let detail = err.as_deref().unwrap_or("");
        let msg = format!(
            "[warden] 健康状态迁移:{name} {prev} → {next}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!("({detail})")
            }
        );
        tracing::warn!("[health] {msg}");
        entry.log.push(
            crate::logs::LogStream::Stderr,
            crate::logs::LEVEL_WARN,
            msg.clone(),
        );
        if let Some(url) = webhook {
            let body = serde_json::json!({
                "service": name,
                "from": prev,
                "to": next,
                "detail": detail,
                "at": chrono::Utc::now().to_rfc3339(),
            });
            // fire-and-forget:告警失败仅记日志,不阻塞检查循环
            tokio::spawn(async move {
                if let Err(e) = reqwest::Client::new()
                    .post(url)
                    .json(&body)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    tracing::warn!("[health] webhook 告警发送失败:{e}");
                }
            });
        }
    }
}
