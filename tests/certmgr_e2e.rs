#![cfg(feature = "reverse-proxy")]
//! P6 证书编排器 e2e:完整 `serve_with_shutdown` daemon + fake-lego 垫片
//!(`[[bin]]`,构建期编译)。覆盖:
//! - GET /cert 状态聚合(证书/lego/https_running)
//! - issue 全链:凭据落盘 → fake-lego 执行 → 证书原子拷贝 → **热重载**
//!   (TLS 握手验新证书:新 CA 信任通过、旧 CA 拒绝)
//! - 冷启动(方案 a):启动时无证书 → https 未跑,签发成功提示需重启
//! - 单任务互斥 409(SLOW 垫片制造在跑窗口)
//! - 手动续期:同一命令加 --renew-force,证书换新

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use warden::config::Config;
use warden::serve_with_shutdown;

const DOMAIN: &str = "cert-e2e.test";
const LEGO: &str = env!("CARGO_BIN_EXE_fake-lego");

// ── 证书生成(rcgen:CA → 叶子,两套独立 CA 证明换证)────────────

struct TestCa {
    params: CertificateParams,
    key: KeyPair,
    root_pem: String,
}

fn new_ca() -> TestCa {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    TestCa {
        params,
        key,
        root_pem: cert.pem(),
    }
}

fn issue_leaf(ca: &TestCa, sans: &[String]) -> (String, String) {
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans
        .iter()
        .map(|s| SanType::DnsName(s.as_str().to_string().try_into().unwrap()))
        .collect();
    let leaf = KeyPair::generate().unwrap();
    let issuer = Issuer::new(ca.params.clone(), &ca.key);
    let cert = params.signed_by(&leaf, &issuer).unwrap();
    (cert.pem(), leaf.serialize_pem())
}

fn wildcard_sans() -> Vec<String> {
    vec![DOMAIN.to_string(), format!("*.{DOMAIN}")]
}

// ── TLS 探测(信任指定根;只关心握手成败与响应可达)──────────────

async fn tls_get(addr: SocketAddr, host: &str, root_pem: &str) -> Result<(u16, String), String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(root_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cfg));
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let sni = ServerName::try_from(host.to_owned()).map_err(|e| e.to_string())?;
    let tls = connector
        .connect(sni, tcp)
        .await
        .map_err(|e| format!("tls: {e}"))?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
            .await
            .map_err(|e| format!("hs: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender
        .send_request(
            hyper::Request::builder()
                .uri(format!("https://{host}/"))
                .header("host", host)
                .body(http_body_util::Empty::<hyper::body::Bytes>::new())
                .unwrap(),
        )
        .await
        .map_err(|e| format!("req: {e}"))?;
    let status = resp.status().as_u16();
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

// ── daemon 装配(真 serve_with_shutdown:AppState.cert 装配 + reloader 注入)──

struct Daemon {
    port: u16,
    https_port: u16,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

struct DaemonOpts<'a> {
    /// 初始证书对(写入 ssl/ 并配进 cert_file/key_file);None = 冷启动。
    initial_cert: Option<(String, String)>,
    /// [proxy.acme] 追加行(测试变体注入)。
    acme_extra: &'a str,
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("warden-cert-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 预占随机端口再让 serve 复绑(获取 https_port 供 TLS 探测)。
async fn pick_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// 放置 fake-lego 预置证书(preset 目录驱动,零环境变量)。
fn write_preset(dir: &Path, cert_pem: &str, key_pem: &str) {
    let preset = dir.join("data/lego/preset");
    std::fs::create_dir_all(&preset).unwrap();
    std::fs::write(preset.join("cert.pem"), cert_pem).unwrap();
    std::fs::write(preset.join("key.pem"), key_pem).unwrap();
}

async fn start_daemon(dir: &Path, opts: DaemonOpts<'_>) -> Daemon {
    let https_port = pick_port().await;
    let mut toml = format!(
        "[daemon]\ndata_dir = {data:?}\nlog_dir = \"\"\n\n[proxy]\ndomain = {DOMAIN:?}\n\
         https_bind = \"127.0.0.1:{https_port}\"\n",
        data = dir.join("data").display().to_string(),
    );
    if let Some((cert, key)) = &opts.initial_cert {
        std::fs::create_dir_all(dir.join("ssl")).unwrap();
        std::fs::write(dir.join("ssl/fullchain.pem"), cert).unwrap();
        std::fs::write(dir.join("ssl/privkey.pem"), key).unwrap();
        toml.push_str(&format!(
            "cert_file = {c:?}\nkey_file = {k:?}\n",
            c = dir.join("ssl/fullchain.pem").display().to_string(),
            k = dir.join("ssl/privkey.pem").display().to_string(),
        ));
    }
    toml.push_str(&format!(
        "\n[proxy.acme]\nlego_path = {LEGO:?}\nemail = \"ops@test.dev\"\n\
         dns_provider = \"fakedns\"\nserver = \"letsencryptstaging\"\n{}",
        opts.acme_extra,
    ));
    let cfg_path = dir.join("services.toml");
    std::fs::write(&cfg_path, &toml).unwrap();

    let cfg = Config::load(Some(&cfg_path)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_with_shutdown(cfg, Some(cfg_path), listener, token).await;
    });
    // 等 API 就绪(health 白名单)
    for _ in 0..40 {
        if (reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/health"))
            .send()
            .await)
            .is_ok()
        {
            return Daemon {
                port,
                https_port,
                shutdown,
                handle,
            };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon API 未就绪");
}

async fn stop(d: Daemon) {
    d.shutdown.cancel();
    let _ = d.handle.await;
}

// ── API 辅助 ─────────────────────────────────────────────────────

async fn req(port: u16, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let url = format!("http://127.0.0.1:{port}{path}");
    let client = reqwest::Client::new();
    let builder = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        _ => panic!("不支持的方法 {method}"),
    };
    let builder = if let Some(v) = body {
        builder.json(&v)
    } else {
        builder
    };
    let resp = builder.send().await.unwrap();
    let status = resp.status().as_u16();
    let value = resp.json::<Value>().await.unwrap_or(Value::Null);
    (status, value)
}

/// 轮询 GET /cert 直到最近任务达到期望状态(超时 panic 带面板尾部)。
async fn wait_task(port: u16, want: &str, secs: u64) -> Value {
    for _ in 0..(secs * 4) {
        let (_, v) = req(port, "GET", "/api/v1/proxy/cert", None).await;
        if v["task"]["status"].as_str() == Some(want) {
            return v["task"].clone();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let (_, v) = req(port, "GET", "/api/v1/proxy/cert", None).await;
    panic!("任务未到 {want}:最新 task = {}", v["task"]);
}

fn joined_output(task: &Value) -> String {
    task["output_tail"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn issue_body() -> Value {
    json!({
        "email": "ops@test.dev",
        "dns_provider": "fakedns",
        "env": { "FAKE_SECRET": "s3cret-value" },
    })
}

// ── 用例 ─────────────────────────────────────────────────────────

/// 意图:issue 全链——凭据 0600 落盘 → fake-lego 执行(双路输出捕获)→
/// 证书原子拷贝 → CertReloader 热重载(TLS 握手:新 CA 通过/旧 CA 拒绝)。
#[tokio::test(flavor = "multi_thread")]
async fn issue_full_chain_with_hot_reload() {
    let dir = tmpdir("issue");
    let ca_old = new_ca();
    let initial = issue_leaf(&ca_old, &wildcard_sans());
    let ca_new = new_ca();
    let fresh = issue_leaf(&ca_new, &wildcard_sans());
    write_preset(&dir, &fresh.0, &fresh.1);
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: Some(initial),
            acme_extra: "",
        },
    )
    .await;

    // 状态聚合:lego 已装(fake --version)、证书在、https 在跑
    let (s, v) = req(d.port, "GET", "/api/v1/proxy/cert", None).await;
    assert_eq!(s, 200);
    assert!(v["lego"]["installed"].as_bool().unwrap(), "{v}");
    assert_eq!(v["lego"]["source"], "explicit");
    assert_eq!(v["lego"]["version"], "0.0.0-fake", "v 前缀应剥掉");
    assert!(v["cert"]["exists"].as_bool().unwrap(), "{v}");
    assert!(v["https_running"].as_bool().unwrap(), "{v}");
    assert!(
        v["cert"]["san"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == "*.cert-e2e.test")),
        "SAN 应含通配:{v}"
    );

    // 一键签发
    let initial_bytes = std::fs::read(dir.join("ssl/fullchain.pem")).unwrap();
    let (s, v) = req(
        d.port,
        "POST",
        "/api/v1/proxy/cert/issue",
        Some(issue_body()),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let task = wait_task(d.port, "success", 20).await;
    let out = joined_output(&task);
    assert!(out.contains("证书已热重载"), "热重载应已触发:{out}");
    assert!(
        out.contains("acme: authorizations okay") && out.contains("fake challenge presented"),
        "stdout/stderr 双路都须进面板:{out}"
    );

    // 证书文件已换新(内容级)
    let new_bytes = std::fs::read(dir.join("ssl/fullchain.pem")).unwrap();
    assert_ne!(initial_bytes, new_bytes, "证书文件应已被替换");
    assert_eq!(new_bytes, fresh.0.as_bytes(), "落盘内容 = fake-lego 产物");

    // TLS 握手验新证书:新 CA 信任通过,旧 CA 拒绝(acceptor 已热切)
    let addr: SocketAddr = format!("127.0.0.1:{}", d.https_port).parse().unwrap();
    let ok = tls_get(addr, DOMAIN, &ca_new.root_pem).await;
    assert!(ok.is_ok(), "新 CA 应握手成功:{:?}", ok.err());
    let stale = tls_get(addr, DOMAIN, &ca_old.root_pem).await;
    assert!(stale.is_err(), "旧 CA 应被拒绝(证书已热切换)");

    // 凭据文件:配置目录(C3)、内容含键值、GET 永不回显
    let env_text = std::fs::read_to_string(dir.join("acme.env")).unwrap();
    assert!(env_text.contains("FAKE_SECRET=s3cret-value"), "{env_text}");
    assert!(!serde_json::to_string(&v).unwrap().contains("s3cret-value"));
    // GET /cert 凭据状态:存在性 + 条目数(键名/值不回显)
    let (_, v) = req(d.port, "GET", "/api/v1/proxy/cert", None).await;
    assert!(v["env"]["exists"].as_bool().unwrap(), "{v}");
    assert_eq!(v["env"]["count"], 1, "{v}");
    assert!(!serde_json::to_string(&v).unwrap().contains("FAKE_SECRET"));

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图(F1):签发凭据**留空 = 沿用既有 acme.env**——已保存凭据的用户重签
/// (如换域名)时表单凭据区留空,不得把 acme.env 清空导致凭据丢失/签发失败;
/// 非空提交才是覆盖写。
#[tokio::test(flavor = "multi_thread")]
async fn issue_empty_env_keeps_existing_credentials() {
    let dir = tmpdir("envkeep");
    let ca = new_ca();
    let initial = issue_leaf(&ca, &wildcard_sans());
    write_preset(&dir, &initial.0, &initial.1);
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: Some(initial),
            acme_extra: "",
        },
    )
    .await;

    // 第一次签发:凭据落盘
    let (s, v) = req(
        d.port,
        "POST",
        "/api/v1/proxy/cert/issue",
        Some(issue_body()),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    wait_task(d.port, "success", 20).await;
    let env_path = dir.join("acme.env");
    assert!(env_path.is_file(), "首次签发凭据应落盘");

    // 第二次签发:凭据留空 → 既有文件原样保留
    let (s, v) = req(
        d.port,
        "POST",
        "/api/v1/proxy/cert/issue",
        Some(json!({
            "email": "ops@test.dev",
            "dns_provider": "fakedns",
            "env": {},
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let task = wait_task(d.port, "success", 20).await;
    let out = joined_output(&task);
    assert!(out.contains("沿用既有"), "留空应在面板明示沿用:{out}");
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("FAKE_SECRET=s3cret-value"),
        "既有凭据不得被空提交清空:{text}"
    );

    // 400 校验路径:坏 email / 坏 provider 字符 / 坏凭据键
    for bad in [
        json!({"email": "no-at-sign", "dns_provider": "fakedns"}),
        json!({"email": "ops@test.dev", "dns_provider": "Bad_Provider"}),
        json!({"email": "ops@test.dev", "dns_provider": "fakedns", "env": {"9bad": "v"}}),
    ] {
        let (s, _) = req(d.port, "POST", "/api/v1/proxy/cert/issue", Some(bad)).await;
        assert_eq!(s, 400, "非法签发请求须 400");
    }

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图(F7):配置写回失败分级——cert_file 已配置时证书本就可用,写回失败
/// 只降级为面板警告(任务成功 + 热重载照常);冷启动(路径未配)仍判失败
/// (那侧写回是重启后 https 能起的前提,由其主路径覆盖)。unix-only:
/// 以只读配置目录制造「读得进、写不出」。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn persist_failure_warns_but_succeeds_when_paths_configured() {
    let dir = tmpdir("persistwarn");
    let ca_old = new_ca();
    let initial = issue_leaf(&ca_old, &wildcard_sans());
    let ca_new = new_ca();
    let fresh = issue_leaf(&ca_new, &wildcard_sans());
    write_preset(&dir, &fresh.0, &fresh.1);
    // 配置放独立子目录(稍后置只读);env_file 显式指到只读目录外
    // (缺省会在步骤 2 写 acme.env 到配置目录,先一步失败干扰用例)
    let cfg_dir = dir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_path = cfg_dir.join("services.toml");
    let https_port = pick_port().await;
    std::fs::write(
        &cfg_path,
        format!(
            "[daemon]\ndata_dir = {d:?}\nlog_dir = \"\"\n\n[proxy]\ndomain = {DOMAIN:?}\n\
             https_bind = \"127.0.0.1:{https_port}\"\ncert_file = {c:?}\nkey_file = {k:?}\n\n\
             [proxy.acme]\nlego_path = {LEGO:?}\nemail = \"ops@test.dev\"\n\
             dns_provider = \"fakedns\"\nenv_file = {e:?}\n",
            d = dir.join("data").display().to_string(),
            c = dir.join("ssl/fullchain.pem").display().to_string(),
            k = dir.join("ssl/privkey.pem").display().to_string(),
            e = dir.join("envs/acme.env").display().to_string(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("ssl")).unwrap();
    std::fs::write(dir.join("ssl/fullchain.pem"), &initial.0).unwrap();
    std::fs::write(dir.join("ssl/privkey.pem"), &initial.1).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    let cfg = Config::load(Some(&cfg_path)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_with_shutdown(cfg, Some(cfg_path), listener, token).await;
    });
    for _ in 0..40 {
        if (reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/health"))
            .send()
            .await)
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (s, v) = req(port, "POST", "/api/v1/proxy/cert/issue", Some(issue_body())).await;
    assert_eq!(s, 200, "{v}");
    let task = wait_task(port, "success", 20).await;
    let out = joined_output(&task);
    assert!(out.contains("配置写回失败"), "写回失败应降级为警告行:{out}");
    assert!(out.contains("证书已热重载"), "证书仍应热重载:{out}");
    assert_eq!(
        std::fs::read(dir.join("ssl/fullchain.pem")).unwrap(),
        fresh.0.as_bytes(),
        "证书应已换新"
    );

    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    shutdown.cancel();
    let _ = handle.await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图(F10):路径漂移——运行期改 cert_file(未重启)后续期,证书按新路径
/// 落盘,但 https reloader 仍监听旧路径:面板须明示「仍监听旧路径,重启生效」
/// (而非误导性的「证书内容未变化」),且 https 实际仍服务旧证书。
#[tokio::test(flavor = "multi_thread")]
async fn renew_reports_path_drift_after_cert_file_change() {
    let dir = tmpdir("drift");
    let ca_old = new_ca();
    let initial = issue_leaf(&ca_old, &wildcard_sans());
    let ca_new = new_ca();
    let fresh = issue_leaf(&ca_new, &wildcard_sans());
    write_preset(&dir, &fresh.0, &fresh.1);
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: Some(initial),
            acme_extra: "",
        },
    )
    .await;

    // 运行期改 cert_file/key_file → ssl2(引擎/shared 不感知,reloader 仍盯 ssl)。
    // start_daemon 以 {:?} 写路径(Windows 反斜杠转义成 \\),替换须用同形态匹配
    let cfg_debug = |p: std::path::PathBuf| format!("{:?}", p.display().to_string());
    let text = std::fs::read_to_string(dir.join("services.toml")).unwrap();
    let text = text
        .replace(
            &cfg_debug(dir.join("ssl/fullchain.pem")),
            &cfg_debug(dir.join("ssl2/fullchain.pem")),
        )
        .replace(
            &cfg_debug(dir.join("ssl/privkey.pem")),
            &cfg_debug(dir.join("ssl2/privkey.pem")),
        );
    std::fs::write(dir.join("services.toml"), text).unwrap();

    let (s, v) = req(d.port, "POST", "/api/v1/proxy/cert/renew", None).await;
    assert_eq!(s, 200, "{v}");
    let task = wait_task(d.port, "success", 20).await;
    let out = joined_output(&task);
    assert!(
        out.contains("仍监听") && out.contains("重启"),
        "漂移须明示监听路径与重启:{out}"
    );
    // 新路径落新证书;旧路径原样(https 仍在服务旧证书)
    assert_eq!(
        std::fs::read(dir.join("ssl2/fullchain.pem")).unwrap(),
        fresh.0.as_bytes()
    );
    let addr: SocketAddr = format!("127.0.0.1:{}", d.https_port).parse().unwrap();
    assert!(
        tls_get(addr, DOMAIN, &ca_old.root_pem).await.is_ok(),
        "https 仍服务旧证书(reloader 盯旧路径)"
    );
    assert!(
        tls_get(addr, DOMAIN, &ca_new.root_pem).await.is_err(),
        "新证书未生效(需重启)"
    );

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图:编排设置面(PUT /cert/acme)——写回 domain(归一剥通配)+ acme 非敏感
/// 字段 + renew_days;GET 反映;空串清空语义;非法输入 400;凭据不经此端点。
#[tokio::test(flavor = "multi_thread")]
async fn acme_settings_put_roundtrip() {
    let dir = tmpdir("cfg");
    let ca = new_ca();
    let initial = issue_leaf(&ca, &wildcard_sans());
    write_preset(&dir, &initial.0, &initial.1);
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: Some(initial),
            acme_extra: "",
        },
    )
    .await;

    // 保存设置:换根域(带通配前缀,应归一)+ 换邮箱/provider/server + 阈值 14
    let (s, v) = req(
        d.port,
        "PUT",
        "/api/v1/proxy/cert/acme",
        Some(json!({
            "email": "newops@x.dev",
            "dns_provider": "Cloudflare",   // 应小写化
            "server": "letsencryptstaging",
            "renew_days": 14,
            "domain": "*.cfg.test",
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["engine"], "live", "引擎在跑,写回应热生效:{v}");

    let cfg_text = std::fs::read_to_string(dir.join("services.toml")).unwrap();
    assert!(cfg_text.contains("domain = \"cfg.test\""), "{cfg_text}");
    assert!(cfg_text.contains("newops@x.dev"), "{cfg_text}");
    assert!(cfg_text.contains("cloudflare"), "{cfg_text}");
    assert!(cfg_text.contains("renew_days = 14"), "{cfg_text}");
    let (_, v) = req(d.port, "GET", "/api/v1/proxy/cert", None).await;
    assert_eq!(v["domain"], "cfg.test", "通配前缀应剥除:{v}");
    assert_eq!(v["acme"]["email"], "newops@x.dev");
    assert_eq!(v["acme"]["dns_provider"], "cloudflare");
    assert_eq!(v["acme"]["renew_days"], 14);
    assert!(v["https_bind"].is_string(), "GET 应透出 https_bind:{v}");
    assert!(!v["env"]["exists"].as_bool().unwrap(), "凭据不经设置面:{v}");

    // 清空:邮箱/provider/根域全空 + renew_days=0(关闭)
    let (s, v) = req(
        d.port,
        "PUT",
        "/api/v1/proxy/cert/acme",
        Some(json!({
            "email": "", "dns_provider": "", "server": "", "renew_days": 0, "domain": "",
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let cfg_text = std::fs::read_to_string(dir.join("services.toml")).unwrap();
    assert!(!cfg_text.contains("cfg.test"), "domain 应已删键:{cfg_text}");
    assert!(!cfg_text.contains("newops@x.dev"), "{cfg_text}");
    let (_, v) = req(d.port, "GET", "/api/v1/proxy/cert", None).await;
    assert_eq!(v["domain"], serde_json::Value::Null, "{v}");
    assert!(!v["acme"]["configured"].as_bool().unwrap(), "{v}");
    assert_eq!(v["acme"]["renew_days"], 0);

    // 校验 400:path / 端口 / 邮箱缺 @ / 阈值超界(`*.` 前缀已被归一剥除,不算非法)
    for bad in [
        json!({"domain": "bad/x.test"}),
        json!({"domain": "a.test:443"}),
        json!({"email": "no-at-sign"}),
        json!({"renew_days": 91}),
    ] {
        let (s, _) = req(d.port, "PUT", "/api/v1/proxy/cert/acme", Some(bad)).await;
        assert_eq!(s, 400, "非法设置须 400");
    }

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图:冷启动(方案 a)——启动时无证书 https 降级,签发成功落盘 +
/// 写回 cert_file/key_file,任务面板明示「重启 warden 后 https 生效」。
#[tokio::test(flavor = "multi_thread")]
async fn cold_start_issue_reports_restart_required() {
    let dir = tmpdir("cold");
    let ca = new_ca();
    let fresh = issue_leaf(&ca, &wildcard_sans());
    write_preset(&dir, &fresh.0, &fresh.1);
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: None,
            acme_extra: "",
        },
    )
    .await;

    let (s, v) = req(d.port, "GET", "/api/v1/proxy/cert", None).await;
    assert_eq!(s, 200);
    assert!(!v["cert"]["exists"].as_bool().unwrap(), "{v}");
    assert!(
        !v["https_running"].as_bool().unwrap(),
        "冷启动 https 未跑:{v}"
    );

    let (s, v) = req(
        d.port,
        "POST",
        "/api/v1/proxy/cert/issue",
        Some(issue_body()),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let task = wait_task(d.port, "success", 20).await;
    let out = joined_output(&task);
    assert!(
        out.contains("重启 warden 后 https 生效"),
        "冷启动须明示重启:{out}"
    );

    // 默认路径落盘 + 配置写回 cert_file/key_file
    let landed = std::fs::read(dir.join("data/ssl/fullchain.pem")).unwrap();
    assert_eq!(landed, fresh.0.as_bytes());
    let cfg_text = std::fs::read_to_string(dir.join("services.toml")).unwrap();
    assert!(
        cfg_text.contains("cert_file"),
        "默认路径场景应写回 cert_file:{cfg_text}"
    );
    assert!(
        cfg_text.contains("dns_provider = \"fakedns\""),
        "编排字段应写回:{cfg_text}"
    );
    // env_file 缺省时不写回绝对路径(缺省本就按配置目录解析;写死伤害目录整体迁移)
    assert!(
        !cfg_text.contains("env_file"),
        "缺省 env_file 不应写回:{cfg_text}"
    );
    // 重读状态:证书现在可见(文件配置已指向落盘路径),https 仍需重启
    let (_, v) = req(d.port, "GET", "/api/v1/proxy/cert", None).await;
    assert!(v["cert"]["exists"].as_bool().unwrap(), "{v}");
    assert!(!v["https_running"].as_bool().unwrap(), "{v}");

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图:单任务互斥——SLOW 垫片制造在跑窗口,第二个任务 409;
/// 结束后可再开。
#[tokio::test(flavor = "multi_thread")]
async fn second_task_conflicts_409_while_running() {
    let dir = tmpdir("conflict");
    let ca = new_ca();
    let initial = issue_leaf(&ca, &wildcard_sans());
    write_preset(&dir, &initial.0, &initial.1);
    // SLOW 标记:fake-lego run 先睡 8s
    std::fs::create_dir_all(dir.join("data/lego")).unwrap();
    std::fs::write(dir.join("data/lego/SLOW"), b"1").unwrap();
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: Some(initial),
            acme_extra: "",
        },
    )
    .await;

    let (s, v) = req(
        d.port,
        "POST",
        "/api/v1/proxy/cert/issue",
        Some(issue_body()),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = req(d.port, "POST", "/api/v1/proxy/cert/renew", None).await;
    assert_eq!(s, 409, "运行中第二个任务须 409:{v}");

    std::fs::remove_file(dir.join("data/lego/SLOW")).unwrap();
    let task = wait_task(d.port, "success", 20).await;
    assert_eq!(task["kind"], "issue");
    // 空闲后可再开
    let (s, v) = req(d.port, "POST", "/api/v1/proxy/cert/renew", None).await;
    assert_eq!(s, 200, "空闲后应可再开:{v}");
    wait_task(d.port, "success", 20).await;

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图:手动续期 = 同一编排加 --renew-force;证书换新并热重载。
#[tokio::test(flavor = "multi_thread")]
async fn manual_renew_replaces_cert() {
    let dir = tmpdir("renew");
    let ca_old = new_ca();
    let initial = issue_leaf(&ca_old, &wildcard_sans());
    let ca_new = new_ca();
    let fresh = issue_leaf(&ca_new, &wildcard_sans());
    write_preset(&dir, &fresh.0, &fresh.1);
    let d = start_daemon(
        &dir,
        DaemonOpts {
            initial_cert: Some(initial),
            acme_extra: "",
        },
    )
    .await;

    let (s, v) = req(d.port, "POST", "/api/v1/proxy/cert/renew", None).await;
    assert_eq!(s, 200, "{v}");
    let task = wait_task(d.port, "success", 20).await;
    assert_eq!(task["kind"], "renew");
    let out = joined_output(&task);
    assert!(
        out.contains("--renew-force"),
        "续期命令须带 --renew-force:{out}"
    );
    assert!(out.contains("证书已热重载"), "{out}");
    assert_eq!(
        std::fs::read(dir.join("ssl/fullchain.pem")).unwrap(),
        fresh.0.as_bytes(),
        "证书应已换新"
    );

    stop(d).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 意图:自动续期(C5)——初始证书 notAfter 拨到 renew_days 内(剩 10 天 <
/// 30),到期检测 task(周期压缩 300ms)驱动 fake-lego 续期落盘,任务面板
/// 记 renew 成功;成功后 24h 冷却不再连发。
#[tokio::test(flavor = "multi_thread")]
async fn auto_renew_triggers_within_threshold() {
    use std::sync::{Arc, Mutex};
    use warden::config::AcmeConfig;
    use warden::proxy::certmgr::{AutoRenew, CertMgr};
    use warden::proxy::tls::{spawn_cert_expiry, CertReloader};

    let dir = tmpdir("auto");
    let ca_old = new_ca();
    // 初始证书:剩余 10 天
    let mut params = CertificateParams::default();
    params.subject_alt_names = wildcard_sans()
        .iter()
        .map(|s| SanType::DnsName(s.as_str().to_string().try_into().unwrap()))
        .collect();
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(10);
    let leaf = KeyPair::generate().unwrap();
    let issuer = Issuer::new(ca_old.params.clone(), &ca_old.key);
    let initial = params.signed_by(&leaf, &issuer).unwrap();
    std::fs::create_dir_all(dir.join("ssl")).unwrap();
    let (cert_p, key_p) = (dir.join("ssl/fullchain.pem"), dir.join("ssl/privkey.pem"));
    std::fs::write(&cert_p, initial.pem()).unwrap();
    std::fs::write(&key_p, leaf.serialize_pem()).unwrap();

    let ca_new = new_ca();
    let fresh = issue_leaf(&ca_new, &wildcard_sans());
    write_preset(&dir, &fresh.0, &fresh.1);

    // 配置文件:编排字段完整 + renew_days=30(AutoRenew 触发时现读)
    let cfg_path = dir.join("services.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[proxy]\ndomain = {DOMAIN:?}\ncert_file = {c:?}\nkey_file = {k:?}\n\n\
             [proxy.acme]\nlego_path = {LEGO:?}\nemail = \"ops@test.dev\"\n\
             dns_provider = \"fakedns\"\nrenew_days = 30\n",
            c = cert_p.display().to_string(),
            k = key_p.display().to_string(),
        ),
    )
    .unwrap();

    // 手工装配(reloader + 编排句柄 + 到期检测 task)
    let mgr = Arc::new(CertMgr::new());
    mgr.set_reloader(Arc::new(CertReloader::new(&cert_p, &key_p).unwrap()));
    let ar = Arc::new(AutoRenew::new(
        mgr.clone(),
        cfg_path.clone(),
        dir.join("data"),
        Arc::new(Mutex::new(())),
    ));
    let shutdown = CancellationToken::new();
    let handle = spawn_cert_expiry(
        cert_p.clone(),
        AcmeConfig {
            renew_days: 30,
            ..Default::default()
        },
        None,
        shutdown.clone(),
        Duration::from_millis(300),
        Some(ar),
    );

    // 断言:证书文件被换成 preset 新证(≤15s)
    let mut replaced = false;
    for _ in 0..60 {
        if std::fs::read(&cert_p)
            .map(|b| b == fresh.0.as_bytes())
            .unwrap_or(false)
        {
            replaced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(replaced, "自动续期应在 renew_days 阈值内触发落盘");

    // 任务面板记录 renew 成功;且冷却生效(1s 内不再产生第二个任务)
    let mut seen_success = false;
    for _ in 0..40 {
        if let Some(t) = mgr.tasks().snapshot(5) {
            if t.kind == "renew" && t.status == "success" {
                seen_success = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(seen_success, "任务面板应记录自动续期成功");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let snap = mgr.tasks().snapshot(5).unwrap();
    assert_eq!(
        (snap.kind, snap.status),
        ("renew", "success"),
        "冷却期内不应再有新任务"
    );

    shutdown.cancel();
    let _ = handle.await;
    let _ = std::fs::remove_dir_all(&dir);
}
