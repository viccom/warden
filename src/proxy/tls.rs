//! TLS 终止(P2)与证书生命周期(P3)。设计见 PLAN-REVERSE-PROXY.md §4.6/Task 10-14。
//!
//! - `CertReloader`:rustls 单证书 acceptor + **mtime 轮询热重载**(续签换证
//!   不停机;失败保留旧配置,只告警不中断服务)。
//! - `spawn_tls_serve`:https 入口——每连接 accept 后走 hyper http1 conn,
//!   ALPN 仅 http/1.1(h2 未启用,与明文入口一致);drain 上限 15s 对齐 mod.rs。
//! - `spawn_cert_reload`:30s(可配,测试压缩)轮询证书对 mtime。
//! - `spawn_cert_expiry` / `cert_expiry_days`(P3):解析 notAfter,剩余低于
//!   阈值告警(tracing + webhook),可选执行外部续期命令。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::Service; // Router::call

use crate::config::AcmeConfig;
use crate::proxy::{forward, ProxyState, DRAIN_LIMIT};

/// 证书热重载周期(生产;测试经 `spawn_cert_reload` 参数压缩)。
pub const CERT_RELOAD_PERIOD: Duration = Duration::from_secs(30);

/// 证书到期检测周期(P3)。
const CERT_CHECK_PERIOD: Duration = Duration::from_secs(3600);

/// 外部续期命令的执行超时(防挂死;acme.sh DNS-01 通常 < 60s,留足余量)。
const RENEW_TIMEOUT: Duration = Duration::from_secs(600);

/// 外部续期命令的最小执行间隔(冷却,防每小时循环反复触发)。
const RENEW_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

/// 单证书 acceptor + mtime 热重载。
pub struct CertReloader {
    cert_path: PathBuf,
    key_path: PathBuf,
    current: RwLock<Arc<rustls::ServerConfig>>,
    /// 最近一次成功加载时的证书对 mtime(变化检测)。
    loaded_sig: Mutex<(SystemTime, SystemTime)>,
}

impl CertReloader {
    /// 加载初始证书对;失败即构造失败(调用方决定降级)。
    pub fn new(cert_path: &Path, key_path: &Path) -> Result<Self, String> {
        let cfg = load_server_config(cert_path, key_path)?;
        let sig = pair_mtime(cert_path, key_path)?;
        Ok(Self {
            cert_path: cert_path.to_path_buf(),
            key_path: key_path.to_path_buf(),
            current: RwLock::new(Arc::new(cfg)),
            loaded_sig: Mutex::new(sig),
        })
    }

    /// 当前配置构建 acceptor(TlsAcceptor 构造成本为一次 Arc clone)。
    pub fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.current.read().expect("cert cfg 锁中毒").clone())
    }

    /// mtime 变化则重载;失败保留旧配置并返回错误文案(调用方记日志)。
    /// 证书对两文件分开落盘的竞态:先到的半个新配对加载失败 → 沿用旧证书,
    /// 下一轮两文件齐了再成功。
    pub fn reload_if_changed(&self) -> Result<bool, String> {
        let sig = pair_mtime(&self.cert_path, &self.key_path)?;
        if sig == *self.loaded_sig.lock().expect("cert sig 锁中毒") {
            return Ok(false);
        }
        let cfg = load_server_config(&self.cert_path, &self.key_path)?;
        *self.current.write().expect("cert cfg 锁中毒") = Arc::new(cfg);
        *self.loaded_sig.lock().expect("cert sig 锁中毒") = sig;
        Ok(true)
    }
}

fn pair_mtime(cert: &Path, key: &Path) -> Result<(SystemTime, SystemTime), String> {
    let m = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|md| md.modified())
            .map_err(|e| format!("读取 {} 元数据失败:{e}", p.display()))
    };
    Ok((m(cert)?, m(key)?))
}

/// 从 PEM 文件对构建 rustls ServerConfig(ALPN 仅 http/1.1)。
pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<rustls::ServerConfig, String> {
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| format!("打开证书 {} 失败:{e}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("解析证书 {} 失败:{e}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!("证书 {} 不含 PEM 证书", cert_path.display()));
    }
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| format!("解析私钥 {} 失败:{e}", key_path.display()))?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("证书/私钥不匹配:{e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

/// 证书热重载轮询 task。
pub fn spawn_cert_reload(
    reloader: Arc<CertReloader>,
    period: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(period) => match reloader.reload_if_changed() {
                    Ok(true) => tracing::info!(
                        "[proxy] 证书已热重载({})",
                        reloader.cert_path.display()
                    ),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("[proxy] 证书热重载失败,沿用旧证书:{e}"),
                },
            }
        }
    })
}

/// https 入口:accept → TLS → hyper http1 conn;优雅停机对齐 mod.rs
/// (每连接挂 graceful shutdown,收口后 JoinSet drain,15s 上限强断)。
pub fn spawn_tls_serve(
    listener: TcpListener,
    reloader: Arc<CertReloader>,
    state: Arc<ProxyState>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        tracing::info!("[proxy] https listening on {local}");
        let router = axum::Router::new()
            .fallback(forward::proxy_handler)
            .with_state(state);
        let mut conns: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        loop {
            // 回收已完成连接的条目:JoinSet 完成项须 join 才移除,不回收则随
            // 累计连接数无界增长(常驻 daemon 内存缓慢上涨)。非阻塞,不影响 accept。
            while conns.try_join_next().is_some() {}
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let acceptor = reloader.acceptor();
                        let router = router.clone();
                        let conn_shutdown = shutdown.clone();
                        conns.spawn(async move {
                            let tls = match acceptor.accept(stream).await {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::debug!("[proxy] tls 握手失败({peer}):{e}");
                                    return;
                                }
                            };
                            // ConnectInfo 手工注入(等价 into_make_service_with_connect_info)
                            let svc = hyper::service::service_fn(
                                move |mut req: hyper::Request<hyper::body::Incoming>| {
                                    req.extensions_mut()
                                        .insert(axum::extract::ConnectInfo::<SocketAddr>(peer));
                                    let mut r = router.clone();
                                    async move { r.call(req).await }
                                },
                            );
                            let conn = hyper::server::conn::http1::Builder::new()
                                .serve_connection(hyper_util::rt::TokioIo::new(tls), svc);
                            tokio::pin!(conn);
                            tokio::select! {
                                _ = conn_shutdown.cancelled() => {
                                    // 通知收尾:当前请求完成后关连接;残余由外层 drain 上限强断
                                    conn.as_mut().graceful_shutdown();
                                    let _ = (&mut conn).await;
                                }
                                r = &mut conn => {
                                    if let Err(e) = r {
                                        tracing::debug!("[proxy] https conn({peer}) 结束:{e}");
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("[proxy] https accept 错误:{e}");
                    }
                },
            }
        }
        // drain:等在途连接收尾(每连接已挂 graceful),上限 15s;JoinSet drop 强断残余
        let deadline = tokio::time::Instant::now() + DRAIN_LIMIT;
        loop {
            if conns.is_empty() {
                break;
            }
            match tokio::time::timeout_at(deadline, conns.join_next()).await {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!(
                        "[proxy] https 连接 {DRAIN_LIMIT:?} 内未全部关闭(WS 隧道/在途请求),强制断开"
                    );
                    break;
                }
            }
        }
        tracing::info!("[proxy] https 已退出");
    })
}

// ── P3:证书到期检测 + 可选续期 ───────────────────────────────────

/// 解析证书文件的剩余有效天数(notAfter - now,向下取整)。
pub fn cert_expiry_days(cert_path: &Path) -> Result<i64, String> {
    let pem =
        std::fs::read(cert_path).map_err(|e| format!("读取 {} 失败:{e}", cert_path.display()))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem)
        .map_err(|e| format!("解析 PEM {} 失败:{e}", cert_path.display()))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| format!("解析证书 {} 失败:{e}", cert_path.display()))?;
    let not_after = cert.validity().not_after.to_datetime().unix_timestamp();
    let now = chrono::Utc::now().timestamp();
    let secs = not_after - now;
    Ok(secs.div_euclid(86_400))
}

/// 剩余天数是否触发告警(阈值含等于不告警;过期负数必然触发)。
fn expiry_should_warn(days: i64, warn_days: u32) -> bool {
    days < warn_days as i64
}

/// 证书到期检测 task(P3):周期解析 notAfter;剩余 < expire_warn_days 时
/// 告警(tracing warn + 可选 webhook,对齐 health 告警形态),并在配置了
/// renew_command 时触发外部续期(冷却 24h,超时强杀)。
/// 每次检测 INFO 一行剩余天数(可观测,便于确认检测链路活着)。
pub fn spawn_cert_expiry(
    cert_path: PathBuf,
    acme: AcmeConfig,
    webhook: Option<String>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_renew: Option<SystemTime> = None;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(CERT_CHECK_PERIOD) => {
                    let days = match cert_expiry_days(&cert_path) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!("[proxy] 证书到期检测失败:{e}");
                            continue;
                        }
                    };
                    tracing::info!("[proxy] 证书剩余 {days} 天({})", cert_path.display());
                    if !expiry_should_warn(days, acme.expire_warn_days) {
                        continue;
                    }
                    // 到期告警(每次检测都发:剩余天数递减,webhook 侧按文案去重)
                    let msg = format!(
                        "[warden] 证书临近过期:剩余 {days} 天(阈值 {} 天),证书 {}",
                        acme.expire_warn_days,
                        cert_path.display()
                    );
                    tracing::warn!("[proxy] {msg}");
                    send_alert(&webhook, "cert_expiry", &msg);
                    // 可选外部续期(冷却 24h,防每小时循环反复执行)
                    if let Some(cmd) = acme.renew_command.as_deref().filter(|c| !c.is_empty()) {
                        let cooled = last_renew.map_or(true, |t| {
                            t.elapsed().map_or(true, |d| d >= RENEW_COOLDOWN)
                        });
                        if !cooled {
                            continue;
                        }
                        match run_renew_command(cmd).await {
                            Ok(out) => {
                                last_renew = Some(SystemTime::now());
                                tracing::info!("[proxy] 续期命令已执行({cmd}):{out}");
                            }
                            Err(e) => {
                                tracing::warn!("[proxy] 续期命令失败({cmd}):{e}");
                            }
                        }
                    }
                }
            }
        }
    })
}

/// fire-and-forget webhook 告警(失败仅记日志;对齐 health 模式)。
fn send_alert(webhook: &Option<String>, kind: &str, msg: &str) {
    let Some(url) = webhook.clone() else { return };
    let body = serde_json::json!({
        "kind": kind,
        "message": msg,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    tokio::spawn(async move {
        if let Err(e) = reqwest::Client::new()
            .post(url)
            .json(&body)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            tracing::warn!("[proxy] webhook 告警发送失败:{e}");
        }
    });
}

/// 执行外部续期命令(unix: sh -c / windows: cmd /C),捕获输出、超时终止。
async fn run_renew_command(cmd: &str) -> Result<String, String> {
    run_renew_command_timeout(cmd, RENEW_TIMEOUT).await
}

async fn run_renew_command_timeout(cmd: &str, timeout: Duration) -> Result<String, String> {
    let mut command = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    // 超时 drop output() future 时连带杀掉子进程(tokio 默认 drop 不杀!)
    command.kill_on_drop(true);
    let out = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("超时({timeout:?}),已终止子进程"))?
        .map_err(|e| format!("spawn/执行失败:{e}"))?;
    let text = format!(
        "exit={} stdout={} stderr={}",
        out.status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into()),
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 意图:剩余天数向下取整——now 之后不足一天的窗口内为 0,
    /// 已过期为负数(告警判断用 < 阈值,负数必然触发)。
    #[test]
    fn expiry_days_floor_and_past() {
        // 构造一个 not_after = now + 90 天 + 1 小时的 PEM 证书
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.not_after =
            time::OffsetDateTime::now_utc() + time::Duration::days(90) + time::Duration::hours(1);
        let cert = params.self_signed(&ca_key).unwrap();
        let dir = std::env::temp_dir().join(format!("warden-exp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.pem");
        std::fs::write(&p, cert.pem()).unwrap();
        assert_eq!(cert_expiry_days(&p).unwrap(), 90, "90 天 + 1 小时 → 90");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图:阈值边界——剩余恰等于阈值不告警(21 天健康,20 天告警),
    /// 负数(已过期)必然告警。
    #[test]
    fn expiry_threshold_boundary() {
        assert!(!expiry_should_warn(21, 21), "剩余=阈值 → 健康");
        assert!(expiry_should_warn(20, 21), "剩余=阈值-1 → 告警");
        assert!(expiry_should_warn(0, 21), "当天到期 → 告警");
        assert!(expiry_should_warn(-3, 21), "已过期 → 告警");
    }

    /// 意图:续期命令成功路径——sh -c 执行、stdout/stderr 捕获、exit=0。
    #[tokio::test]
    async fn renew_command_success_captures_output() {
        // sh 用 ';' 分隔;cmd 不认 ';'(会整串回显),须用 '&' + 1>&2
        #[cfg(unix)]
        let cmd = "echo renew-ok; echo warn-line >&2";
        #[cfg(windows)]
        let cmd = "echo renew-ok & echo warn-line 1>&2";
        let out = run_renew_command(cmd).await.unwrap();
        assert!(out.contains("exit=0"), "exit 码捕获:{out}");
        assert!(out.contains("renew-ok"), "stdout 捕获:{out}");
        assert!(out.contains("warn-line"), "stderr 捕获:{out}");
    }

    /// 意图:续期命令失败路径——非零 exit 报 Err,内容含 exit 码与 stderr。
    #[tokio::test]
    async fn renew_command_failure_reports_exit_code() {
        #[cfg(unix)]
        let cmd = "echo boom >&2; exit 3";
        // cmd:'&' 分隔 + exit /b 设置 errorlevel(';' 会被 echo 整体回显)
        #[cfg(windows)]
        let cmd = "echo boom 1>&2 & exit /b 3";
        let err = run_renew_command(cmd).await.unwrap_err();
        assert!(err.contains("exit=3"), "exit 码上报:{err}");
        assert!(err.contains("boom"), "stderr 带回:{err}");
    }

    /// 意图(Y1):续期命令超时必须**终止子进程**——tokio 默认 drop Child 不杀
    /// (kill_on_drop=false),挂死的续期脚本会残留且每小时再起一个。
    /// 判别法:`sleep 1; touch marker` 配 150ms 超时——被杀则 marker 永不出现,
    /// 泄漏则 ~1s 后出现。
    #[tokio::test]
    async fn renew_command_timeout_kills_child() {
        let marker = std::env::temp_dir().join(format!("warden-renewkill-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        // 命令:延时 ~1s 后落 marker。unix 用 sleep/touch;windows 用 ping 延时
        // (cmd 无 sleep)+ type nul 建文件
        #[cfg(unix)]
        let cmd = format!("sleep 1; touch {}", marker.to_string_lossy());
        #[cfg(windows)]
        let cmd = format!(
            "ping -n 2 127.0.0.1 >nul & type nul > \"{}\"",
            marker.to_string_lossy()
        );
        let r = run_renew_command_timeout(&cmd, Duration::from_millis(150)).await;
        assert!(r.is_err(), "超时应报 Err:{r:?}");
        // 等 shell 的 sleep 走完(若未被杀,marker 会在 ~1s 出现)
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(
            !marker.exists(),
            "超时后子进程须被终止,marker 不应出现(进程泄漏)"
        );
    }

    /// 意图:热重载失败路径——证书对被写坏后 reload 报 Err 且**沿用旧配置**
    /// (acceptor 不变,服务不中断);这是换证竞态半个新配对的兜底行为。
    #[test]
    fn reload_failure_keeps_old_config() {
        let ck = rcgen::generate_simple_self_signed(vec!["*.x.example.com".to_string()]).unwrap();
        let dir = std::env::temp_dir().join(format!("warden-reloadfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("fullchain.pem");
        let key = dir.join("privkey.pem");
        std::fs::write(&cert, ck.cert.pem()).unwrap();
        std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();

        let reloader = CertReloader::new(&cert, &key).unwrap();
        // 未变化 → Ok(false)
        assert!(!reloader.reload_if_changed().unwrap());

        // 写坏证书(PEM 非法)并显式拨动 mtime——同 tick 内重写 mtime 可能不变
        // (tmpfs 时钟粒度),set_modified 模拟"时间已过"确保变化被检测
        std::fs::write(&cert, "not a pem at all").unwrap();
        let f = std::fs::File::options().write(true).open(&cert).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1))
            .unwrap();
        let r = reloader.reload_if_changed();
        assert!(r.is_err(), "坏证书对须报错:{r:?}");
        // acceptor 仍可构建(旧配置未被破坏)
        let _acceptor = reloader.acceptor();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
