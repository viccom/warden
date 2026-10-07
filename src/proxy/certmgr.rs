//! 证书编排器(P6):warden 编排 lego 实现证书一键申请/续期/安装。
//!
//! 设计见 docs/PLAN-CERT-ORCHESTRATOR.md。职责分工(D13 演进):
//! ACME/DNS-01 协议由 lego 外部二进制执行,本模块只做**编排**——
//! 任务管理(单任务互斥 + 流式输出环形缓冲)、lego 检测/GitHub 安装、
//! 签发/续期命令构造与执行、证书落盘与热重载触发、配置写回。
//! DNS 凭据存独立 0600 的 acme.env(决策 C3,不入主配置),API 永不回显。
//!
//! 热重载边界(方案 a):daemon 启动时证书已存在 https 才在跑,续期落盘后
//! 热生效;首次部署签发成功时无 CertReloader 可触发,需重启 daemon。
//!
//! 执行器按「可注入」组织(`LegoRunner`):单测用内存假实现,e2e 走真实
//! spawn(fake-lego 垫片,tests/helpers/fake_lego.rs)。

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::config::AcmeConfig;
use crate::lock::{lock, read, write};
use crate::proxy::tls::CertReloader;

/// lego 任务执行超时(DNS-01 传播可达分钟级,对齐 renew_command 语义)。
pub const LEGO_TIMEOUT: Duration = Duration::from_secs(600);

/// 任务输出环形缓冲上限(行)。
const OUTPUT_CAP: usize = 500;

/// lego GitHub 仓库(latest release 资产来源,C2)。
pub const LEGO_REPO: &str = "go-acme/lego";

/// 自动安装下载的体积上限(lego 资产 ~21MB;防配错镜像/损坏源灌满磁盘)。
pub const DOWNLOAD_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// lego 状态缓存时长(避免任务面板轮询每次都 spawn `--version`)。
const LEGO_CACHE_TTL: Duration = Duration::from_secs(30);

// ── 任务管理器 ────────────────────────────────────────────────────

/// 任务种类(编排动作)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Install,
    Issue,
    Renew,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Issue => "issue",
            Self::Renew => "renew",
        }
    }
}

/// 任务状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Success,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
        }
    }
}

/// 最近一次任务的对外快照(API/任务面板消费)。
#[derive(Clone, Debug)]
pub struct CertTaskSnapshot {
    pub kind: &'static str,
    pub status: &'static str,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit: Option<String>,
    pub message: Option<String>,
    /// 输出尾部(最多 `tail` 行;环形缓冲满后 total_lines > output_tail.len)。
    pub output_tail: Vec<String>,
    pub total_lines: usize,
}

#[derive(Debug)]
struct CertTask {
    kind: TaskKind,
    status: TaskStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    exit: Option<String>,
    message: Option<String>,
    output: VecDeque<String>,
}

/// 证书任务管理器:同一时间仅一个任务(互斥冲突由 API 层转 409),
/// 保留最近一次任务(含输出尾部)供观测。
#[derive(Default)]
pub struct CertTaskManager {
    inner: Mutex<Option<CertTask>>,
}

impl CertTaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始新任务;已有任务在跑 → Err(占用方摘要,调用方转 409)。
    pub fn try_begin(&self, kind: TaskKind) -> Result<(), String> {
        let mut g = lock(&self.inner);
        if let Some(t) = g.as_ref().filter(|t| t.status == TaskStatus::Running) {
            return Err(format!(
                "证书任务进行中({},{})",
                t.kind.as_str(),
                t.started_at.format("%H:%M:%S")
            ));
        }
        *g = Some(CertTask {
            kind,
            status: TaskStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            exit: None,
            message: None,
            output: VecDeque::with_capacity(64),
        });
        Ok(())
    }

    /// 追加一行输出(仅 Running 任务接收;超环形上限丢最旧)。
    pub fn push_line(&self, line: &str) {
        let mut g = lock(&self.inner);
        let Some(t) = g.as_mut().filter(|t| t.status == TaskStatus::Running) else {
            return;
        };
        if t.output.len() == OUTPUT_CAP {
            t.output.pop_front();
        }
        t.output.push_back(line.to_string());
    }

    /// 结束任务(仅 Running 时生效——迟到的回调不改写已完成任务)。
    pub fn finish(&self, ok: bool, exit: Option<String>, message: Option<String>) {
        let mut g = lock(&self.inner);
        let Some(t) = g.as_mut().filter(|t| t.status == TaskStatus::Running) else {
            return;
        };
        t.status = if ok {
            TaskStatus::Success
        } else {
            TaskStatus::Failed
        };
        t.finished_at = Some(Utc::now());
        t.exit = exit;
        t.message = message;
    }

    /// 是否有任务在跑。
    pub fn is_running(&self) -> bool {
        lock(&self.inner)
            .as_ref()
            .is_some_and(|t| t.status == TaskStatus::Running)
    }

    /// 最近一次任务快照(从未有任务 → None)。
    pub fn snapshot(&self, tail: usize) -> Option<CertTaskSnapshot> {
        let g = lock(&self.inner);
        g.as_ref().map(|t| CertTaskSnapshot {
            kind: t.kind.as_str(),
            status: t.status.as_str(),
            started_at: t.started_at,
            finished_at: t.finished_at,
            exit: t.exit.clone(),
            message: t.message.clone(),
            total_lines: t.output.len(),
            output_tail: t.output.iter().rev().take(tail).rev().cloned().collect(),
        })
    }
}

// ── 执行器(可注入)──────────────────────────────────────────────

/// 单次执行结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecOutcome {
    pub success: bool,
    pub exit: Option<i32>,
    pub timed_out: bool,
    /// spawn/等待失败描述(区分「没装」与「跑挂」)。
    pub spawn_error: Option<String>,
}

/// 输出行接收器(任务面板实时行)。
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

/// lego 执行器抽象:生产 spawn 真进程;单测注入假实现。
#[async_trait]
pub trait LegoRunner: Send + Sync {
    /// 执行 `program args`,stdout/stderr 逐行回调 `sink`,返回退出结果。
    async fn run(&self, program: &Path, args: &[String], sink: &LineSink) -> ExecOutcome;
}

/// 真实进程执行器:流式行捕获(区别于 tls::run_renew_command 的全量缓冲——
/// 观测场景需要行级实时),`kill_on_drop` 对齐 Y1 修复,超时显式 kill。
pub struct ProcessRunner {
    pub timeout: Duration,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            timeout: LEGO_TIMEOUT,
        }
    }
}

#[async_trait]
impl LegoRunner for ProcessRunner {
    async fn run(&self, program: &Path, args: &[String], sink: &LineSink) -> ExecOutcome {
        use std::process::Stdio;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 超时 drop run future 时连带杀子进程(tokio 默认 drop 不杀)
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ExecOutcome {
                    spawn_error: Some(format!("{}: {e}", program.display())),
                    ..Default::default()
                }
            }
        };
        // stdout/stderr 两路读行 task(sink 逐行回调,EOF 自然退出)
        let mut readers = Vec::new();
        if let Some(s) = child.stdout.take() {
            readers.push(spawn_line_reader(s, sink.clone()));
        }
        if let Some(s) = child.stderr.take() {
            readers.push(spawn_line_reader(s, sink.clone()));
        }
        let outcome = match tokio::time::timeout(self.timeout, child.wait()).await {
            // 超时:Child 尚未被 drop(kill_on_drop 不生效),显式杀
            Err(_) => {
                let _ = child.kill().await;
                ExecOutcome {
                    timed_out: true,
                    ..Default::default()
                }
            }
            Ok(Err(e)) => ExecOutcome {
                spawn_error: Some(e.to_string()),
                ..Default::default()
            },
            Ok(Ok(status)) => ExecOutcome {
                success: status.success(),
                exit: status.code(),
                ..Default::default()
            },
        };
        for r in readers {
            let _ = r.await;
        }
        outcome
    }
}

/// 单路输出流读行 task(UTF-8 宽松解码,行尾 CR/LF 剥离;EOF 自然退出)。
fn spawn_line_reader<S>(stream: S, sink: LineSink) -> tokio::task::JoinHandle<()>
where
    S: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let mut r = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => sink(String::from_utf8_lossy(&buf).trim_end()),
            }
        }
    })
}

/// 收集输出执行(`--version` 等需要拿内容的场景)。
pub async fn run_capture(
    runner: &dyn LegoRunner,
    program: &Path,
    args: &[String],
) -> (ExecOutcome, Vec<String>) {
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink: LineSink = {
        let lines = lines.clone();
        Arc::new(move |l: &str| lock(&lines).push(l.to_string()))
    };
    let out = runner.run(program, args, &sink).await;
    let collected = lock(&lines).clone();
    (out, collected)
}

/// 执行结果的一行描述(任务面板/错误信息用)。
fn exec_desc(out: &ExecOutcome) -> String {
    if out.timed_out {
        return format!("超时(>{})", LEGO_TIMEOUT.as_secs());
    }
    if let Some(e) = &out.spawn_error {
        return format!("spawn 失败:{e}");
    }
    match out.exit {
        Some(c) => format!("exit={c}"),
        None => "被信号终止".into(),
    }
}

// ── lego 检测 ─────────────────────────────────────────────────────

/// lego 命中来源(检测顺序:Explicit > DataDir > Path,决策 C2)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegoSource {
    Explicit,
    DataDir,
    Path,
}

impl LegoSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::DataDir => "data_dir",
            Self::Path => "path",
        }
    }
}

/// lego 状态视图(GET /api/v1/proxy/cert 消费)。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LegoStatus {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub source: Option<LegoSource>,
}

fn lego_exe_name() -> &'static str {
    if cfg!(windows) {
        "lego.exe"
    } else {
        "lego"
    }
}

/// 检测候选路径(纯函数):显式 `lego_path` → `<data_dir>/bin/` → PATH 各目录。
pub fn lego_candidates(
    lego_path: Option<&str>,
    data_dir: &Path,
    path_env: Option<&std::ffi::OsStr>,
) -> Vec<(PathBuf, LegoSource)> {
    let mut v = Vec::new();
    if let Some(p) = lego_path.map(str::trim).filter(|s| !s.is_empty()) {
        v.push((PathBuf::from(p), LegoSource::Explicit));
    }
    v.push((
        data_dir.join("bin").join(lego_exe_name()),
        LegoSource::DataDir,
    ));
    if let Some(dirs) = path_env.map(std::env::split_paths) {
        for d in dirs.filter(|d| !d.as_os_str().is_empty()) {
            v.push((d.join(lego_exe_name()), LegoSource::Path));
        }
    }
    v
}

/// 逐候选验证(`--version` 可执行即命中),返回 lego 状态。
pub async fn resolve_lego(
    runner: &dyn LegoRunner,
    lego_path: Option<&str>,
    data_dir: &Path,
) -> LegoStatus {
    resolve_lego_with(runner, lego_path, data_dir, std::env::var_os("PATH")).await
}

/// 同 [`resolve_lego`],PATH 由调用方传入(测试可控)。
pub async fn resolve_lego_with(
    runner: &dyn LegoRunner,
    lego_path: Option<&str>,
    data_dir: &Path,
    path_env: Option<std::ffi::OsString>,
) -> LegoStatus {
    for (path, source) in lego_candidates(lego_path, data_dir, path_env.as_deref()) {
        if !path.is_file() {
            continue;
        }
        let (out, lines) = run_capture(runner, &path, &["--version".to_string()]).await;
        if out.success {
            return LegoStatus {
                installed: true,
                path: Some(path.display().to_string()),
                version: lines.first().map(|l| parse_lego_version(l)),
                source: Some(source),
            };
        }
        tracing::warn!(
            "[cert] 候选 {} 存在但不可执行({}),继续检测",
            path.display(),
            exec_desc(&out)
        );
    }
    LegoStatus::default()
}

/// 从 lego `--version` 首行取版本号(真 lego 形如 `lego version 5.5.2
/// linux/amd64`——版本 token 无 `v` 前缀;垫片输出 `v0.0.0-fake`)。
fn parse_lego_version(line: &str) -> String {
    let mut it = line.split_whitespace();
    while let Some(tok) = it.next() {
        if tok.eq_ignore_ascii_case("version") {
            if let Some(v) = it.next() {
                return v.trim_start_matches('v').to_string();
            }
        }
    }
    // 回退:首个数字开头的 token;再退整行
    line.split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(line.trim())
        .to_string()
}

// ── 证书概要(GET /cert 消费)──────────────────────────────────

/// 证书文件概要:剩余天数/notAfter/SAN。
pub struct CertSummary {
    pub days_left: i64,
    pub not_after: Option<String>,
    pub san: Vec<String>,
}

/// 解析证书文件(取首个 PEM 块即叶子证书;链文件后续块忽略)。
pub fn cert_summary(path: &Path) -> Result<CertSummary, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取 {} 失败:{e}", path.display()))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&bytes)
        .map_err(|e| format!("解析 PEM {} 失败:{e}", path.display()))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| format!("解析证书 {} 失败:{e}", path.display()))?;
    let not_after = cert.validity().not_after.to_datetime().unix_timestamp();
    let days_left = (not_after - chrono::Utc::now().timestamp()).div_euclid(86_400);
    let not_after = chrono::DateTime::from_timestamp(not_after, 0).map(|dt| dt.to_rfc3339());
    let san = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|ext| {
            ext.value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    x509_parser::extensions::GeneralName::DNSName(s) => Some(s.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(CertSummary {
        days_left,
        not_after,
        san,
    })
}

// ── 编排器共享句柄 ───────────────────────────────────────────────
/// 编排器共享句柄:AppState(feature 门控字段)与到期检测 task 共用。
/// 证书热重载触发(https 启动后注入 reloader)+ lego 状态缓存。
pub struct CertMgr {
    tasks: CertTaskManager,
    reloader: RwLock<Option<Arc<CertReloader>>>,
    lego_cache: Mutex<Option<(Instant, LegoStatus)>>,
}

impl Default for CertMgr {
    fn default() -> Self {
        Self {
            tasks: CertTaskManager::new(),
            reloader: RwLock::new(None),
            lego_cache: Mutex::new(None),
        }
    }
}

impl CertMgr {
    pub fn new() -> Self {
        Self::default()
    }

    /// https 入口启动成功后注入(lib.rs 装配处);未注入 = https 未跑(冷启动)。
    pub fn set_reloader(&self, r: Arc<CertReloader>) {
        *write(&self.reloader) = Some(r);
    }

    /// https 入口是否在跑(决定签发成功后能否热生效)。
    pub fn https_running(&self) -> bool {
        read(&self.reloader).is_some()
    }

    /// https 入口监听的证书路径(None = https 未跑)。编排器用于检测
    /// 「落盘路径 ≠ 监听路径」漂移(运行期改 cert_file 未重启场景)。
    pub fn watching_cert_path(&self) -> Option<PathBuf> {
        read(&self.reloader)
            .as_ref()
            .map(|r| r.cert_path().to_path_buf())
    }

    /// 任务管理器引用。
    pub fn tasks(&self) -> &CertTaskManager {
        &self.tasks
    }

    /// lego 状态(30s 缓存;install 后须 invalidate)。
    pub async fn lego_status(
        &self,
        runner: &dyn LegoRunner,
        acme: &AcmeConfig,
        data_dir: &Path,
    ) -> LegoStatus {
        if let Some((at, st)) = lock(&self.lego_cache).as_ref() {
            if at.elapsed() < LEGO_CACHE_TTL {
                return st.clone();
            }
        }
        let st = resolve_lego(runner, acme.lego_path.as_deref(), data_dir).await;
        *lock(&self.lego_cache) = Some((Instant::now(), st.clone()));
        st
    }

    pub fn invalidate_lego_cache(&self) {
        *lock(&self.lego_cache) = None;
    }

    /// 触发证书热重载;https 未跑 → None(方案 a:调用方提示重启)。
    pub fn reload_cert(&self) -> Option<Result<bool, String>> {
        read(&self.reloader).as_ref().map(|r| r.reload_if_changed())
    }
}

/// 自动续期冷却(仅成功计 24h;失败按到期检测周期重试,对齐 renew_command)。
pub const AUTO_RENEW_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

/// 自动续期句柄(到期检测 task 持有,lib.rs 装配;C5 优先级):
/// lego 编排配置完整(email + dns_provider)时由 warden 驱动续期,
/// `renew_command` 被忽略;判定/参数**触发时现读文件**(运行期改配置即时生效)。
pub struct AutoRenew {
    mgr: Arc<CertMgr>,
    config_path: PathBuf,
    data_dir: PathBuf,
    edit_lock: Arc<Mutex<()>>,
    last_success: Arc<Mutex<Option<Instant>>>,
}

impl AutoRenew {
    pub fn new(
        mgr: Arc<CertMgr>,
        config_path: PathBuf,
        data_dir: PathBuf,
        edit_lock: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            mgr,
            config_path,
            data_dir,
            edit_lock,
            last_success: Arc::new(Mutex::new(None)),
        }
    }

    /// 当前文件中的 [proxy](读失败 → None)。
    fn file_proxy(&self) -> Option<crate::config::ProxyConfig> {
        crate::config::Config::load(Some(&self.config_path))
            .ok()
            .and_then(|c| c.proxy)
    }

    /// lego 编排配置是否完整(触发时现读;编排完整 = renew_command 被忽略,C5)。
    pub fn lego_ready(&self) -> bool {
        self.file_proxy()
            .is_some_and(|p| p.acme.email.is_some() && p.acme.dns_provider.is_some())
    }

    /// 冷却判定(24h,仅成功计)。
    fn cooled(&self) -> bool {
        lock(&self.last_success).map_or(true, |t| t.elapsed() >= AUTO_RENEW_COOLDOWN)
    }

    /// 到期检测周期调用:内部完成全部判定(现读配置/阈值 renew_days/冷却/单任务
    /// 互斥)并在满足时后台触发续期。renew_days=0 = 关闭自动续期。
    pub fn check_and_trigger(&self, days: i64) {
        let Some(pc) = self.file_proxy() else {
            return;
        };
        let a = &pc.acme;
        if a.email.is_none() || a.dns_provider.is_none() {
            return; // 编排不完整:到期检测的 renew_command 兜底继续负责
        }
        if a.renew_days == 0 || days >= i64::from(a.renew_days) {
            return; // 未到阈值 / 已关闭
        }
        if !self.cooled() {
            return;
        }
        if self.mgr.tasks().try_begin(TaskKind::Renew).is_err() {
            tracing::warn!("[cert] 自动续期跳过:证书任务进行中(下周期重试)");
            return;
        }
        let mgr = self.mgr.clone();
        let last = self.last_success.clone();
        let ctx = OrchestratorCtx {
            data_dir: self.data_dir.clone(),
            config_path: self.config_path.clone(),
            acme: a.clone(),
            cert_file: pc.cert_file.clone(),
            key_file: pc.key_file.clone(),
            domain: pc.domain.clone(),
            edit_lock: self.edit_lock.clone(),
        };
        tracing::info!(
            "[cert] 剩余 {days} 天 < renew_days {},自动续期触发",
            a.renew_days
        );
        tokio::spawn(async move {
            let ok = execute_renew(mgr.clone(), Arc::new(ProcessRunner::default()), ctx).await;
            if ok {
                *lock(&last) = Some(Instant::now());
                tracing::info!("[cert] 自动续期成功(进入 24h 冷却)");
            } else {
                tracing::warn!("[cert] 自动续期失败,按到期检测周期重试");
            }
        });
    }
}

// ── 凭据文件(acme.env)─────────────────────────────────────────

/// 单行 KEY=VALUE(dotenv 惯例:值含空白/引号/# 时双引号包裹,内部引号转义)。
fn env_line(k: &str, v: &str) -> String {
    let needs_quote = v.is_empty() || v.chars().any(|c| c.is_whitespace() || c == '"' || c == '#');
    let val = if needs_quote {
        format!("\"{}\"", v.replace('"', "\\\""))
    } else {
        v.to_string()
    };
    format!("{k}={val}")
}

/// 校验凭据键值(键 `[A-Za-z_][A-Za-z0-9_]*`;键值均禁换行——防注入多行)。
pub fn validate_env_entry(k: &str, v: &str) -> Result<(), String> {
    let mut cs = k.chars();
    let head_ok = cs
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !(head_ok && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        return Err(format!("凭据键名 '{k}' 不合法(须 [A-Za-z_][A-Za-z0-9_]*)"));
    }
    if k.contains('\n') || v.contains('\n') || v.contains('\r') {
        return Err(format!("凭据键值含换行:{k}"));
    }
    Ok(())
}

/// 写 acme.env(覆盖式;父目录自动创建)。内容永不打日志。
/// 原子落盘(tmp + rename,中断不留半文件);unix **显式收敛 0600**——
/// `OpenOptions::mode` 仅在创建时生效,预存在且权限过宽的文件(如手工 touch
/// 过的 0644)不会因覆盖写收窄,凭据可能落成可读,故写后统一 chmod。
pub fn write_env_file(path: &Path, env: &BTreeMap<String, String>) -> Result<(), String> {
    for (k, v) in env {
        validate_env_entry(k, v)?;
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败:{e}", dir.display()))?;
        }
    }
    let mut text = String::with_capacity(env.len() * 32);
    for (k, v) in env {
        text.push_str(&env_line(k, v));
        text.push('\n');
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|e| format!("写 {} 失败:{e}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    {
        // Windows 无 0600(ACL 从简,文档注明);尽力写入 tmp 再 rename
        std::fs::write(&tmp, &text).map_err(|e| format!("写 {} 失败:{e}", tmp.display()))?;
    }
    // rename 覆盖既存文件(unix 原生 / Windows 走 MoveFileExW REPLACE_EXISTING);
    // 失败清理 tmp 残留
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("落盘 {} 失败:{e}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 {} 权限 0600 失败:{e}", path.display()))?;
    }
    Ok(())
}

/// acme.env 路径解析:显式 `env_file`(相对路径按进程 cwd,启动时已锚定配置
/// 基准目录)→ 缺省 `<配置文件目录>/acme.env`(决策 C3,2026-10-07 修订)。
pub fn resolve_env_file(acme: &AcmeConfig, config_path: &Path) -> PathBuf {
    acme.env_file.as_deref().map_or_else(
        || {
            config_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("acme.env")
        },
        PathBuf::from,
    )
}

// ── 签发/续期编排 ─────────────────────────────────────────────────

/// 编排上下文(API handler 与到期检测 task 在**触发时**组装——参数现读,
/// 不缓存;运行期改配置即时生效,计划 §5.4)。
pub struct OrchestratorCtx {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub acme: AcmeConfig,
    /// [proxy] cert_file/key_file(均 None = 落默认路径并写回配置)。
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    /// [proxy] 根域(domains 缺省时推导 `[裸域, *.裸域]`)。
    pub domain: Option<String>,
    /// 配置写回互斥锁(与 CRUD 同锁,照 routes_proxy 事务模板)。
    pub edit_lock: Arc<Mutex<()>>,
}

/// 签发请求参数(POST /cert/issue body)。
pub struct IssueParams {
    pub email: String,
    pub dns_provider: String,
    pub server: Option<String>,
    pub domains: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    /// 成功后是否写回 [proxy.acme](默认 true;一次性签发可关)。
    pub persist: bool,
}

/// lego 工作目录(账户/证书产物:`<data_dir>/lego`)。
pub fn lego_home(data_dir: &Path) -> PathBuf {
    data_dir.join("lego")
}

/// 默认证书落盘路径(未配置 cert_file/key_file 时)。
pub fn default_cert_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    let dir = data_dir.join("ssl");
    (dir.join("fullchain.pem"), dir.join("privkey.pem"))
}

/// 签发域名:显式传入(小写化 + 保序去重,裸域提到首位)或从根域推导。
/// 裸域必须第一——lego 按首个域名命名产物文件且通配符替换为 `_`;
/// 去重防 `-d a -d a` 直传(LE 拒绝重复 SAN)。
pub fn derive_domains(
    explicit: Option<Vec<String>>,
    domain: Option<&str>,
) -> Result<Vec<String>, String> {
    match explicit {
        Some(list) if !list.is_empty() => {
            let mut out: Vec<String> = Vec::with_capacity(list.len());
            for d in list {
                let d = d.to_lowercase();
                if !out.contains(&d) {
                    out.push(d);
                }
            }
            Ok(order_apex_first(out))
        }
        _ => {
            let Some(root) = domain
                .map(|d| d.trim_start_matches("*.").trim())
                .filter(|d| !d.is_empty())
            else {
                return Err("未提供 domains 且 [proxy] 未配置 domain,无法推导签发域名".into());
            };
            Ok(vec![root.to_string(), format!("*.{root}")])
        }
    }
}

/// 首个非通配域名提到首位(已是裸域开头则原样;全通配则不动)。
fn order_apex_first(mut domains: Vec<String>) -> Vec<String> {
    if domains.first().is_some_and(|d| !d.starts_with("*.")) {
        return domains;
    }
    if let Some(i) = domains.iter().position(|d| !d.starts_with("*.")) {
        let apex = domains.remove(i);
        domains.insert(0, apex);
    }
    domains
}

/// lego 产物文件名(首个域名,`*` 替换为 `_`)。
fn lego_cert_paths(lego_home: &Path, primary: &str) -> (PathBuf, PathBuf) {
    let name = primary.replace('*', "_");
    let dir = lego_home.join("certificates");
    (
        dir.join(format!("{name}.crt")),
        dir.join(format!("{name}.key")),
    )
}

/// `lego run` 参数(纯函数;`--no-random-sleep` 为编排决策:观测性优先,
/// 防风暴由 warden 侧 1h 周期 + 24h 冷却承担,不依赖 lego 随机延迟)。
pub fn lego_run_args(
    server: &str,
    email: &str,
    dns: &str,
    domains: &[String],
    home: &Path,
    env_file: &Path,
    renew_force: bool,
) -> Vec<String> {
    let mut a = vec![
        "run".to_string(),
        "--accept-tos".to_string(),
        "--email".to_string(),
        email.to_string(),
        "--server".to_string(),
        server.to_string(),
        "--dns".to_string(),
        dns.to_string(),
    ];
    for d in domains {
        a.push("-d".to_string());
        a.push(d.clone());
    }
    a.extend([
        "--path".to_string(),
        home.display().to_string(),
        "--env-file".to_string(),
        env_file.display().to_string(),
        "--no-random-sleep".to_string(),
    ]);
    if renew_force {
        a.push("--renew-force".to_string());
    }
    a
}

/// 原子拷贝(临时文件 + rename;父目录自动创建)。
fn atomic_copy(src: &Path, dst: &Path) -> Result<(), String> {
    let bytes = std::fs::read(src).map_err(|e| format!("读取 {} 失败:{e}", src.display()))?;
    if let Some(d) = dst.parent() {
        if !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d).map_err(|e| format!("创建 {} 失败:{e}", d.display()))?;
        }
    }
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写 {} 失败:{e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("落盘 {} 失败:{e}", dst.display()))
}

/// 编排任务异常终止兜底:execute_* 跑在无人 await 的 `tokio::spawn` 里,
/// panic 会被静默吞掉、任务停在 Running → 所有证书端点 409 锁死到重启。
/// 正常路径都会显式 finish(非 Running 时 Drop 判定 no-op),因此 Drop 时
/// 仍 Running 只可能是 panic/unwind,强制置失败并留痕。
struct RunningGuard {
    mgr: Arc<CertMgr>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if self.mgr.tasks().is_running() {
            tracing::error!("[cert] 编排任务异常终止(panic),已强制置为失败");
            self.mgr.tasks().finish(
                false,
                None,
                Some("任务异常终止(内部错误,详见 warden 日志)".into()),
            );
        }
    }
}

/// 执行签发任务(handler 已 try_begin(Issue) 并 spawn 本函数)。
/// 返回任务成败(AutoRenew 冷却记账复用)。
pub async fn execute_issue(
    mgr: Arc<CertMgr>,
    runner: Arc<dyn LegoRunner>,
    ctx: OrchestratorCtx,
    params: IssueParams,
) -> bool {
    let _panic_guard = RunningGuard { mgr: mgr.clone() };
    let m = mgr.clone();
    let sink: LineSink = Arc::new(move |l: &str| m.tasks().push_line(l));
    let server = params
        .server
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ctx.acme.server.clone());
    match run_lego_flow(
        mgr.as_ref(),
        runner.as_ref(),
        &sink,
        &ctx,
        TaskKind::Issue,
        &params.email,
        &params.dns_provider,
        &server,
        params.domains.clone(),
        Some(&params.env),
        params.persist,
    )
    .await
    {
        Ok(msg) => {
            mgr.tasks().finish(true, Some("exit=0".into()), Some(msg));
            true
        }
        Err(e) => {
            mgr.tasks().finish(false, None, Some(e));
            false
        }
    }
}

/// 执行手动/自动续期任务(调用方已 try_begin(Renew) 并 spawn 本函数);
/// email/dns/server 取当前配置,不重写凭据文件,不写回配置。
/// 返回任务成败(AutoRenew 冷却记账复用)。
pub async fn execute_renew(
    mgr: Arc<CertMgr>,
    runner: Arc<dyn LegoRunner>,
    ctx: OrchestratorCtx,
) -> bool {
    let _panic_guard = RunningGuard { mgr: mgr.clone() };
    let m = mgr.clone();
    let sink: LineSink = Arc::new(move |l: &str| m.tasks().push_line(l));
    let (email, dns) = match (&ctx.acme.email, &ctx.acme.dns_provider) {
        (Some(e), Some(d)) => (e.clone(), d.clone()),
        (None, _) => {
            mgr.tasks().finish(
                false,
                None,
                Some("未配置 [proxy.acme] email,无法续期".into()),
            );
            return false;
        }
        (_, None) => {
            mgr.tasks().finish(
                false,
                None,
                Some("未配置 [proxy.acme] dns_provider,无法续期".into()),
            );
            return false;
        }
    };
    match run_lego_flow(
        mgr.as_ref(),
        runner.as_ref(),
        &sink,
        &ctx,
        TaskKind::Renew,
        &email,
        &dns,
        &ctx.acme.server,
        None,
        None,
        false,
    )
    .await
    {
        Ok(msg) => {
            mgr.tasks().finish(true, Some("exit=0".into()), Some(msg));
            true
        }
        Err(e) => {
            mgr.tasks().finish(false, None, Some(e));
            false
        }
    }
}

/// 签发/续期共用流程:解析 lego →(可选)写凭据 → 构造命令 → 执行 →
/// 证书原子拷贝落盘 →(可选)配置写回 → 触发热重载。
/// 返回 Err 时任务面板已含过程输出,Err 即 finish 的失败摘要。
#[expect(clippy::too_many_arguments)]
async fn run_lego_flow(
    mgr: &CertMgr,
    runner: &dyn LegoRunner,
    sink: &LineSink,
    ctx: &OrchestratorCtx,
    kind: TaskKind,
    email: &str,
    dns: &str,
    server: &str,
    domains: Option<Vec<String>>,
    env: Option<&BTreeMap<String, String>>,
    persist: bool,
) -> Result<String, String> {
    // 1. lego 解析(不走缓存:install 刚完成需立即可见)
    let lego = resolve_lego(runner, ctx.acme.lego_path.as_deref(), &ctx.data_dir).await;
    let Some(lego_path) = lego.path.filter(|_| lego.installed) else {
        return Err(
            "lego 未安装:请先「自动安装」或手动放置(检测顺序:lego_path → data/bin → PATH)".into(),
        );
    };

    // 2. 凭据文件:**非空覆盖写**(空 = 沿用既有 acme.env——签发表单凭据留空
    //    重签时不误清空已保存凭据);renew(env=None)不重写
    let env_file = resolve_env_file(&ctx.acme, &ctx.config_path);
    match env {
        Some(env) if !env.is_empty() => {
            write_env_file(&env_file, env).map_err(|e| format!("写凭据文件失败:{e}"))?;
            sink(&format!(
                "凭据已写入 {}({} 项,0600)",
                env_file.display(),
                env.len()
            ));
        }
        Some(_) if env_file.is_file() => {
            sink(&format!("凭据留空:沿用既有 {}", env_file.display()));
        }
        Some(_) => sink(&format!(
            "警告:未提供凭据且 {} 不存在(需要凭据的 DNS provider 将签发失败)",
            env_file.display()
        )),
        None => {}
    }

    // 3. 命令构造(裸域第一)与执行
    let home = lego_home(&ctx.data_dir);
    let domains = derive_domains(domains, ctx.domain.as_deref())?;
    sink(&format!("域名:{domains:?}"));
    let args = lego_run_args(
        server,
        email,
        dns,
        &domains,
        &home,
        &env_file,
        kind == TaskKind::Renew,
    );
    sink(&format!("$ {} {}", lego_path, args.join(" ")));
    let out = runner.run(Path::new(&lego_path), &args, sink).await;
    let desc = exec_desc(&out);
    sink(&format!("lego 结束:{desc}"));
    if !out.success {
        return Err(format!("lego 执行失败({desc}),过程输出见上方"));
    }

    // 4. 证书原子拷贝落盘(产物 → cert_file/key_file 或默认路径)
    let (src_crt, src_key) = lego_cert_paths(&home, &domains[0]);
    let (tgt_crt, tgt_key) = match (&ctx.cert_file, &ctx.key_file) {
        (Some(c), Some(k)) => (PathBuf::from(c), PathBuf::from(k)),
        _ => default_cert_paths(&ctx.data_dir),
    };
    atomic_copy(&src_crt, &tgt_crt)?;
    atomic_copy(&src_key, &tgt_key)?;
    sink(&format!(
        "证书已落盘 {} / {}",
        tgt_crt.display(),
        tgt_key.display()
    ));

    // 5. 写回配置(锁内,照 routes_proxy 事务模板:锁 → 编辑 → save)。
    //    失败分级:cert_file 原本未配(冷启动,路径写回是重启后 https 能起的前提)
    //    → 判任务失败;路径已配 → 仅告警(证书已就绪,元数据缺写不阻碍生效)。
    if persist {
        let essential = ctx.cert_file.is_none();
        let _edit = lock(&ctx.edit_lock);
        let r = (|| -> Result<(), String> {
            let mut file = crate::config_edit::ConfigFile::load_or_create(&ctx.config_path)
                .map_err(|e| format!("加载配置失败:{e}"))?;
            if ctx.cert_file.is_none() {
                file.set_proxy_cert_paths(&tgt_crt, &tgt_key)
                    .map_err(|e| format!("写回证书路径失败:{e}"))?;
            }
            file.upsert_proxy_acme(&crate::config_edit::AcmePersist {
                email: Some(email.to_string()),
                dns_provider: Some(dns.to_string()),
                server: Some(server.to_string()),
                // 仅显式配置时回写原值;缺省路径本就按配置目录动态解析,
                // 写回绝对路径会让部署目录整体迁移后指到旧位置(F9)
                env_file: ctx.acme.env_file.clone(),
                renew_days: None, // 签发流程不动自动续期阈值(设置面负责)
            })
            .map_err(|e| format!("写回 [proxy.acme] 失败:{e}"))?;
            file.save().map_err(|e| format!("保存配置失败:{e}"))
        })();
        match r {
            Ok(()) => sink(&format!("配置已写回 {}", ctx.config_path.display())),
            Err(e) if essential => {
                return Err(format!(
                    "{e}(证书已落盘,但路径写回失败——修复配置后重试,或手补 cert_file/key_file)"
                ));
            }
            Err(e) => sink(&format!(
                "警告:配置写回失败:{e}(证书已就绪不影响生效;[proxy.acme] 元数据未更新)"
            )),
        }
    }

    // 6. 热重载(方案 a:https 未跑 → 提示重启;落盘路径与监听路径漂移
    //    (运行期改过 cert_file 未重启)→ 同样提示重启,不再误报「未变化」)
    if let Some(w) = mgr.watching_cert_path().filter(|w| *w != tgt_crt) {
        sink(&format!(
            "证书已落盘 {},但 https 仍监听 {}(cert_file 运行期已变更)——重启 warden 后生效",
            tgt_crt.display(),
            w.display()
        ));
    } else {
        match mgr.reload_cert() {
            Some(Ok(true)) => sink("证书已热重载(https 即时生效)"),
            Some(Ok(false)) => sink("证书内容未变化(沿用现配置)"),
            Some(Err(e)) => sink(&format!("热重载失败:{e}(30s 轮询会重试)")),
            None => sink("https 入口未运行:重启 warden 后 https 生效(首次签发边界,见 AGENT-GUIDE)"),
        }
    }
    mgr.invalidate_lego_cache();
    Ok(format!("证书已就绪:{}", tgt_crt.display()))
}

/// 平台 → lego release 资产名(纯函数;tag 形如 `v5.6.0`)。
pub fn lego_asset_name(os: &str, arch: &str, tag: &str) -> Option<String> {
    let os = match os {
        "linux" => "linux",
        "windows" => "windows",
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    Some(format!("lego_{tag}_{os}_{arch}.{ext}"))
}

// ── lego 自动安装(GitHub release,C2;版本规格 + 备用镜像)──────────

/// 自动安装的版本规格(`[proxy.acme] lego_version`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegoVersionSpec {
    /// GitHub latest(缺省)。
    Latest,
    /// 主版本跟踪:`5` → 该主版本最新(推荐的固定方式,大版本内兼容)。
    Major(u32),
    /// 精确 tag(归一带 `v` 前缀)。
    Exact(String),
}

/// 解析版本规格(纯函数):空/`latest` → Latest;纯数字(可带 v)→ Major;
/// 其余(含点版本串)→ Exact(补 v 前缀)。
/// 溢出 u32 的纯数字串按 Exact 处理——查询会明确报 404,而非静默回退
/// latest 改变语义(配置笔误须大声失败)。
pub fn parse_version_spec(s: Option<&str>) -> LegoVersionSpec {
    let t = s
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("latest"));
    match t {
        None => LegoVersionSpec::Latest,
        Some(v) => {
            let bare = v.trim_start_matches('v');
            if !bare.is_empty() && bare.chars().all(|c| c.is_ascii_digit()) {
                match bare.parse::<u32>() {
                    Ok(n) => LegoVersionSpec::Major(n),
                    Err(_) => LegoVersionSpec::Exact(format!("v{bare}")),
                }
            } else {
                LegoVersionSpec::Exact(format!("v{bare}"))
            }
        }
    }
}

/// tag 的主版本号(`v5.5.2` → 5;无 v 前缀也认)。
fn tag_major(tag: &str) -> Option<u32> {
    tag.strip_prefix('v')?.split('.').next()?.parse().ok()
}

/// 从 releases 列表(最新在前)选指定主版本的最新 tag(纯函数)。
pub fn pick_major_tag(tags: &[String], major: u32) -> Option<String> {
    tags.iter().find(|t| tag_major(t) == Some(major)).cloned()
}

/// 渲染备用镜像 URL 模板(`{tag}`/`{asset}` 占位符替换;纯函数)。
pub fn render_mirror_url(tpl: &str, tag: &str, asset: &str) -> String {
    tpl.replace("{tag}", tag).replace("{asset}", asset)
}

/// 按版本规格解析 release:返回 (tag, 资产名, GitHub 直连下载 URL)。
pub async fn resolve_lego_release(
    client: &reqwest::Client,
    spec: &LegoVersionSpec,
) -> Result<(String, String, String), String> {
    let url = match spec {
        LegoVersionSpec::Latest => {
            format!("https://api.github.com/repos/{LEGO_REPO}/releases/latest")
        }
        LegoVersionSpec::Exact(t) => {
            format!("https://api.github.com/repos/{LEGO_REPO}/releases/tags/{t}")
        }
        LegoVersionSpec::Major(_) => {
            format!("https://api.github.com/repos/{LEGO_REPO}/releases?per_page=100")
        }
    };
    let desc = match spec {
        LegoVersionSpec::Latest => "latest".into(),
        LegoVersionSpec::Major(n) => format!("v{n} 主版本"),
        LegoVersionSpec::Exact(t) => t.clone(),
    };
    let resp = client
        .get(&url)
        .header("User-Agent", format!("warden/{}", crate::VERSION))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| {
            format!("查询 GitHub release({desc})失败:{e}(内网不可达见 AGENT-GUIDE 手动放置)")
        })?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API 状态 {}({desc})", resp.status()));
    }
    let meta: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 release 元数据失败:{e}"))?;
    // 主版本跟踪:列表里挑该主版本最新一条;其余形态:单对象/按 tag 命中
    let (tag, assets) = match spec {
        LegoVersionSpec::Major(n) => {
            let arr = meta.as_array().ok_or("releases 响应不是数组")?;
            let tags: Vec<String> = arr
                .iter()
                .filter_map(|r| r["tag_name"].as_str().map(String::from))
                .collect();
            let t = pick_major_tag(&tags, *n)
                .ok_or_else(|| format!("releases 列表无 v{n} 主版本(近 100 个 release 内)"))?;
            let rel = arr
                .iter()
                .find(|r| r["tag_name"].as_str() == Some(t.as_str()))
                .expect("tag 取自同列表");
            (t, rel["assets"].clone())
        }
        _ => {
            let t = meta["tag_name"]
                .as_str()
                .ok_or("响应缺 tag_name")?
                .to_string();
            (t, meta["assets"].clone())
        }
    };
    let asset =
        lego_asset_name(std::env::consts::OS, std::env::consts::ARCH, &tag).ok_or_else(|| {
            format!(
                "无 {}({}) 的资产映射",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
    let dl = assets
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|it| it["name"].as_str() == Some(asset.as_str()))
        })
        .and_then(|it| it["browser_download_url"].as_str())
        .ok_or_else(|| format!("release({tag})无资产 {asset}"))?
        .to_string();
    Ok((tag, asset, dl))
}

/// 下载 + 系统 tar 解包 + 安装到 `<data_dir>/bin/`(任务输出经 sink)。
/// 解压零新依赖:调系统 tar(Windows 10 1803+ 自带 bsdtar 可解 zip)。
/// 版本按 `lego_version` 规格;GitHub 直连失败且配了 `lego_mirror` 时按
/// 模板回退重试。
pub async fn install_lego(
    runner: &dyn LegoRunner,
    client: &reqwest::Client,
    acme: &AcmeConfig,
    data_dir: &Path,
    sink: &LineSink,
) -> Result<LegoStatus, String> {
    let tmp = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建 {} 失败:{e}", tmp.display()))?;
    let spec = parse_version_spec(acme.lego_version.as_deref());
    let (tag, asset, dl) = resolve_lego_release(client, &spec).await?;
    // 下载候选:GitHub 直连优先,配置了镜像模板则追加备用
    let mut urls = vec![dl];
    if let Some(tpl) = acme
        .lego_mirror
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        urls.push(render_mirror_url(tpl, &tag, &asset));
    }
    sink(&format!("下载 lego {tag}({asset};{} 个下载源)", urls.len()));
    let archive = tmp.join("lego_archive");
    download_first_available(client, &urls, &archive, DOWNLOAD_MAX_BYTES, sink).await?;
    sink("解包(系统 tar)…");
    let extract = tmp.join("extract");
    let _ = std::fs::remove_dir_all(&extract);
    std::fs::create_dir_all(&extract)
        .map_err(|e| format!("创建 {} 失败:{e}", extract.display()))?;
    let status = tokio::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&extract)
        .output()
        .await
        .map_err(|e| format!("tar 启动失败:{e}(Windows 需 10 1803+)"))?;
    if !status.status.success() {
        return Err(format!(
            "tar 解包失败:{}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    let found = find_file(&extract, lego_exe_name()).ok_or("解包产物中未找到 lego 二进制")?;
    let bin = data_dir.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("创建 {} 失败:{e}", bin.display()))?;
    let dst = bin.join(lego_exe_name());
    // tmp + rename 原子替换(中途断电/崩溃不留半个坏二进制;Windows 上若
    // 目标 lego 正在运行,替换会失败报错即止)
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::copy(&found, &tmp).map_err(|e| format!("安装到 {} 失败:{e}", dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod +x 失败:{e}"))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("安装到 {} 失败:{e}", dst.display()));
    }
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&extract);
    sink(&format!("已安装 {}(lego {tag})", dst.display()));
    // 验证可执行
    let (out, lines) = run_capture(runner, &dst, &["--version".to_string()]).await;
    if !out.success {
        return Err(format!("安装后 --version 验证失败({})", exec_desc(&out)));
    }
    Ok(LegoStatus {
        installed: true,
        path: Some(dst.display().to_string()),
        version: lines.first().map(|l| parse_lego_version(l)),
        source: Some(LegoSource::DataDir),
    })
}

/// 流式下载到文件(报告字节数;超过 `max_bytes` 立即中止——防配错镜像/
/// 损坏源灌满磁盘,正常 lego 资产 ~21MB)。
async fn download_stream(
    client: &reqwest::Client,
    url: &str,
    dst: &Path,
    max_bytes: u64,
    sink: &LineSink,
) -> Result<(), String> {
    let mut resp = client
        .get(url)
        .header("User-Agent", format!("warden/{}", crate::VERSION))
        .send()
        .await
        .map_err(|e| format!("下载失败:{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载状态 {}", resp.status()));
    }
    let mut file = tokio::fs::File::create(dst)
        .await
        .map_err(|e| format!("创建 {} 失败:{e}", dst.display()))?;
    use tokio::io::AsyncWriteExt;
    let mut total = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("下载中断:{e}"))? {
        total += chunk.len() as u64;
        if total > max_bytes {
            return Err(format!(
                "超过大小上限 {max_bytes} 字节(疑似非 lego 资产或损坏下载源)"
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("写 {} 失败:{e}", dst.display()))?;
    }
    file.flush().await.map_err(|e| format!("flush 失败:{e}"))?;
    sink(&format!("下载完成({total} 字节)"));
    Ok(())
}

/// 依次尝试候选 URL 下载(每个失败经 sink 记录;全失败 → 最后一个错误)。
/// 备用镜像语义:候选 = [GitHub 直连, 镜像模板渲染]。
async fn download_first_available(
    client: &reqwest::Client,
    urls: &[String],
    dst: &Path,
    max_bytes: u64,
    sink: &LineSink,
) -> Result<(), String> {
    let mut last = String::new();
    for u in urls {
        match download_stream(client, u, dst, max_bytes, sink).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                sink(&format!("下载源失败 {u}:{e}"));
                last = e;
            }
        }
    }
    Err(format!("全部下载源失败:{last}"))
}

/// 解包目录中按文件名递归查找(资产结构平铺,防御嵌套;目录小)。
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut queue = VecDeque::from([dir.to_path_buf()]);
    while let Some(d) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                queue.push_back(p);
            } else if p.file_name().is_some_and(|n| n == name) {
                return Some(p);
            }
        }
    }
    None
}

/// 执行安装任务(handler 已 try_begin(Install) 并 spawn 本函数)。
/// 返回任务成败。
pub async fn execute_install(
    mgr: Arc<CertMgr>,
    runner: Arc<dyn LegoRunner>,
    acme: AcmeConfig,
    data_dir: PathBuf,
) -> bool {
    let _panic_guard = RunningGuard { mgr: mgr.clone() };
    let m = mgr.clone();
    let sink: LineSink = Arc::new(move |l: &str| m.tasks().push_line(l));
    match install_lego(
        runner.as_ref(),
        &reqwest::Client::new(),
        &acme,
        &data_dir,
        &sink,
    )
    .await
    {
        Ok(st) => {
            mgr.invalidate_lego_cache();
            mgr.tasks().finish(
                true,
                Some("exit=0".into()),
                Some(format!("lego 已安装({})", st.version.unwrap_or_default())),
            );
            true
        }
        Err(e) => {
            mgr.tasks().finish(false, None, Some(e));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假执行器:回放预置行与退出码(单测用,不 spawn)。
    struct FakeRunner {
        exit: i32,
        lines: Vec<&'static str>,
    }

    #[async_trait]
    impl LegoRunner for FakeRunner {
        async fn run(&self, _program: &Path, _args: &[String], sink: &LineSink) -> ExecOutcome {
            for l in &self.lines {
                sink(l);
            }
            ExecOutcome {
                success: self.exit == 0,
                exit: Some(self.exit),
                ..Default::default()
            }
        }
    }

    fn sink_to_vec(v: &Arc<Mutex<Vec<String>>>) -> LineSink {
        let v = v.clone();
        Arc::new(move |l: &str| lock(v.as_ref()).push(l.to_string()))
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warden-certmgr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 意图:单任务互斥——Running 时 try_begin 拒绝;结束后可再开;
    /// finish 对已完成任务是 no-op(迟到回调不改写结果)。
    #[test]
    fn task_manager_single_task_and_finish_noop() {
        let m = CertTaskManager::new();
        assert!(!m.is_running());
        m.try_begin(TaskKind::Issue).unwrap();
        assert!(m.is_running());
        assert!(
            m.try_begin(TaskKind::Renew).is_err(),
            "Running 中须拒绝新任务"
        );

        m.push_line("line-1");
        m.finish(true, Some("exit=0".into()), Some("ok".into()));
        assert!(!m.is_running(), "结束后不再 Running");

        m.finish(false, None, Some("late".into())); // 迟到回调:no-op
        let snap = m.snapshot(10).unwrap();
        assert_eq!(snap.status, "success", "迟到 finish 不得改写");
        assert_eq!(snap.message.as_deref(), Some("ok"));
        assert_eq!(snap.output_tail, vec!["line-1".to_string()]);

        m.try_begin(TaskKind::Install).unwrap(); // 空闲后可再开
        let snap = m.snapshot(10).unwrap();
        assert_eq!((snap.kind, snap.status), ("install", "running"));
    }

    /// 意图:输出环形缓冲封顶 500 行,丢最旧保最新。
    #[test]
    fn task_output_ring_buffer_caps_at_500() {
        let m = CertTaskManager::new();
        m.try_begin(TaskKind::Issue).unwrap();
        for i in 0..600 {
            m.push_line(&format!("l{i}"));
        }
        let snap = m.snapshot(10).unwrap();
        assert_eq!(snap.total_lines, OUTPUT_CAP);
        assert_eq!(snap.output_tail.first().map(String::as_str), Some("l590"));
        assert_eq!(snap.output_tail.last().map(String::as_str), Some("l599"));
        // tail 截断:snapshot(2) 只取尾部 2 行
        assert_eq!(m.snapshot(2).unwrap().output_tail.len(), 2);
    }

    /// 意图:检测候选顺序——显式 > data_dir/bin > PATH(C2),空 PATH 项跳过。
    #[test]
    fn lego_candidates_order() {
        let data = Path::new("/data");
        let path_env: std::ffi::OsString = if cfg!(windows) {
            "C:\\a;C:\\b".into()
        } else {
            "/usr/local/bin:/usr/bin".into()
        };
        let cands = lego_candidates(Some("/opt/lego"), data, Some(path_env.as_os_str()));
        let kinds: Vec<LegoSource> = cands.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            kinds,
            vec![
                LegoSource::Explicit,
                LegoSource::DataDir,
                LegoSource::Path,
                LegoSource::Path
            ]
        );
        assert_eq!(cands[0].0, PathBuf::from("/opt/lego"));
        let expect_bin = if cfg!(windows) {
            "\\data\\bin\\lego.exe"
        } else {
            "/data/bin/lego"
        };
        assert!(cands[1].0.ends_with(expect_bin), "{:?}", cands[1].0);
        // 无显式时 data_dir 优先
        let cands2 = lego_candidates(None, data, None);
        assert_eq!(cands2.len(), 1);
        assert_eq!(cands2[0].1, LegoSource::DataDir);
    }

    /// 意图:平台资产名映射(linux tar.gz / windows zip / arm64;未知平台 None)。
    #[test]
    fn lego_asset_name_mapping() {
        assert_eq!(
            lego_asset_name("linux", "x86_64", "v5.6.0"),
            Some("lego_v5.6.0_linux_amd64.tar.gz".into())
        );
        assert_eq!(
            lego_asset_name("windows", "x86_64", "v5.6.0"),
            Some("lego_v5.6.0_windows_amd64.zip".into())
        );
        assert_eq!(
            lego_asset_name("linux", "aarch64", "v5.6.0"),
            Some("lego_v5.6.0_linux_arm64.tar.gz".into())
        );
        assert_eq!(lego_asset_name("freebsd", "x86_64", "v5.6.0"), None);
    }

    /// 意图:版本规格解析——缺省/latest → Latest;纯数字(可带 v)→ Major;
    /// 版本串 → Exact(补 v 前缀归一)。
    #[test]
    fn parse_version_spec_shapes() {
        use LegoVersionSpec::*;
        assert_eq!(parse_version_spec(None), Latest);
        assert_eq!(parse_version_spec(Some("")), Latest);
        assert_eq!(parse_version_spec(Some(" latest ")), Latest);
        assert_eq!(parse_version_spec(Some("Latest")), Latest);
        assert_eq!(parse_version_spec(Some("5")), Major(5));
        assert_eq!(parse_version_spec(Some(" v5 ")), Major(5));
        assert_eq!(parse_version_spec(Some("5.5.2")), Exact("v5.5.2".into()));
        assert_eq!(parse_version_spec(Some("v5.5.2")), Exact("v5.5.2".into()));
        // 溢出 u32 的纯数字串:按精确 tag(查询报错),不得静默回退 latest
        assert_eq!(
            parse_version_spec(Some("99999999999")),
            Exact("v99999999999".into())
        );
    }

    /// 意图:主版本跟踪——列表(最新在前)取该主版本最新;无 v 前缀也认;
    /// 不存在 → None。
    #[test]
    fn pick_major_tag_prefers_newest() {
        let tags: Vec<String> = ["v6.0.0", "v5.9.1", "v5.5.2", "v4.23.0", "5.1.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_major_tag(&tags, 5).as_deref(), Some("v5.9.1"));
        assert_eq!(pick_major_tag(&tags, 6).as_deref(), Some("v6.0.0"));
        assert_eq!(pick_major_tag(&tags, 4).as_deref(), Some("v4.23.0"));
        assert_eq!(pick_major_tag(&tags, 7), None);
    }

    /// 意图:镜像模板渲染——{tag}/{asset} 占位符替换。
    #[test]
    fn render_mirror_url_substitutes() {
        assert_eq!(
            render_mirror_url(
                "https://gh-proxy.com/https://github.com/go-acme/lego/releases/download/{tag}/{asset}",
                "v5.5.2",
                "lego_v5.5.2_linux_amd64.tar.gz"
            ),
            "https://gh-proxy.com/https://github.com/go-acme/lego/releases/download/v5.5.2/lego_v5.5.2_linux_amd64.tar.gz"
        );
    }

    /// 意图:下载候选回退——首源 404 时落第二源内容;失败经 sink 记录。
    /// 本地起真 HTTP 服务(/bad → 404,/good → 200 + 字节)。
    #[tokio::test]
    async fn download_falls_back_to_second_source() {
        use std::sync::Mutex as StdMutex;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = b"lego-archive-bytes";
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 2048];
                let _n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf).to_string();
                let resp = if req.starts_with("GET /bad") {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        let dir = tmpdir("dlfb");
        let dst = dir.join("archive");
        let got = Arc::new(StdMutex::new(Vec::new()));
        let sink = sink_to_vec(&got);
        let urls = vec![
            format!("http://{addr}/bad/lego.tar.gz"),
            format!("http://{addr}/good/lego.tar.gz"),
        ];
        download_first_available(
            &reqwest::Client::new(),
            &urls,
            &dst,
            DOWNLOAD_MAX_BYTES,
            &sink,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), body);
        let lines = lock(got.as_ref()).clone();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("/bad") && l.contains("404")),
            "首源失败应记录:{lines:?}"
        );
        // 单源失败 → Err 带最后错误
        let only_bad = vec![format!("http://{addr}/bad/lego.tar.gz")];
        let r = download_first_available(
            &reqwest::Client::new(),
            &only_bad,
            &dst,
            DOWNLOAD_MAX_BYTES,
            &sink,
        )
        .await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图(F8):下载体积上限——响应体超过 max_bytes 立即中止报错,
    /// 防配错镜像/损坏源灌满磁盘。本地真 HTTP 服务返回超限响应体。
    #[tokio::test]
    async fn download_stream_enforces_size_cap() {
        use std::sync::Mutex as StdMutex;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = vec![b'x'; 1024];
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        let dir = tmpdir("cap");
        let dst = dir.join("archive");
        let got = Arc::new(StdMutex::new(Vec::new()));
        let sink = sink_to_vec(&got);
        let r = download_stream(
            &reqwest::Client::new(),
            &format!("http://{addr}/big.tar.gz"),
            &dst,
            512,
            &sink,
        )
        .await;
        let err = r.expect_err("超过上限应报错");
        assert!(err.contains("上限"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图:域名推导——缺省 [裸域, *.裸域];显式通配在前自动纠偏裸域首位
    /// (lego 产物文件名规则);全无 → 报错。
    #[test]
    fn derive_domains_rules() {
        let d = derive_domains(None, Some("gxai.site")).unwrap();
        assert_eq!(d, vec!["gxai.site".to_string(), "*.gxai.site".to_string()]);
        // domain 带通配前缀也归一
        assert_eq!(derive_domains(None, Some("*.gxai.site")).unwrap(), d);
        // 显式:通配在前 → 裸域提到首位
        let d2 =
            derive_domains(Some(vec!["*.GXAI.site".into(), "gxai.site".into()]), None).unwrap();
        assert_eq!(
            d2,
            vec!["gxai.site".to_string(), "*.gxai.site".to_string()],
            "裸域须排第一且小写化"
        );
        // 全通配:原样(产物文件名 _ 前缀由 lego_cert_paths 处理)
        assert_eq!(
            derive_domains(Some(vec!["*.a.site".into()]), None).unwrap(),
            vec!["*.a.site".to_string()]
        );
        // 去重保序(大小写归一后视为相同;LE 拒绝重复 SAN)
        assert_eq!(
            derive_domains(
                Some(vec![
                    "a.site".into(),
                    "A.SITE".into(),
                    "*.a.site".into(),
                    "a.site".into()
                ]),
                None
            )
            .unwrap(),
            vec!["a.site".to_string(), "*.a.site".to_string()]
        );
        assert!(derive_domains(None, None).is_err());
        assert!(derive_domains(Some(vec![]), None).is_err());
    }

    /// 意图:lego 产物路径按首个域名命名,通配符替换为 `_`。
    #[test]
    fn lego_cert_paths_wildcard_underscore() {
        let (crt, key) = lego_cert_paths(Path::new("/d/lego"), "gxai.site");
        assert!(crt.ends_with("certificates/gxai.site.crt"));
        assert!(key.ends_with("certificates/gxai.site.key"));
        let (c2, _) = lego_cert_paths(Path::new("/d/lego"), "*.gxai.site");
        assert!(c2.ends_with("certificates/_.gxai.site.crt"));
    }

    /// 意图:命令构造完整且顺序稳定(裸域第一、--no-random-sleep、
    /// renew 加 --renew-force;凭据只经 env 文件,命令行无秘密)。
    #[test]
    fn lego_run_args_shape() {
        let domains = vec!["gxai.site".to_string(), "*.gxai.site".to_string()];
        let a = lego_run_args(
            "letsencrypt",
            "ops@x.com",
            "tencentcloud",
            &domains,
            Path::new("/d/lego"),
            Path::new("/d/config/acme.env"),
            false,
        );
        assert_eq!(a[0], "run");
        assert_eq!(a[1], "--accept-tos");
        assert!(a.windows(2).any(|w| w == ["-d", "gxai.site"]));
        // 裸域在通配之前
        let apex = a.iter().position(|x| x == "gxai.site").unwrap();
        let wild = a.iter().position(|x| x == "*.gxai.site").unwrap();
        assert!(apex < wild);
        assert!(a.contains(&"--no-random-sleep".to_string()));
        assert!(!a.contains(&"--renew-force".to_string()));
        assert!(a
            .windows(2)
            .any(|w| w == ["--env-file", "/d/config/acme.env"]));

        let a2 = lego_run_args(
            "letsencrypt",
            "ops@x.com",
            "tencentcloud",
            &domains,
            Path::new("/d/lego"),
            Path::new("/d/config/acme.env"),
            true,
        );
        assert_eq!(a2.last().map(String::as_str), Some("--renew-force"));
    }

    /// 意图:dotenv 行格式——纯值裸写,含空白/引号/# 时双引号包裹转义。
    #[test]
    fn env_line_formatting() {
        assert_eq!(env_line("K", "v"), "K=v");
        assert_eq!(env_line("K", ""), "K=\"\"");
        assert_eq!(env_line("K", "a b"), "K=\"a b\"");
        assert_eq!(env_line("K", "a#b"), "K=\"a#b\"");
        assert_eq!(env_line("K", "s\"q"), "K=\"s\\\"q\"");
    }

    /// 意图:凭据键值校验——合法键通过,非法键/换行拒绝(防注入多行)。
    #[test]
    fn validate_env_entry_rules() {
        assert!(validate_env_entry("TENCENTCLOUD_SECRET_ID", "x").is_ok());
        assert!(validate_env_entry("_k9", "x").is_ok());
        assert!(validate_env_entry("9k", "x").is_err());
        assert!(validate_env_entry("a-b", "x").is_err());
        assert!(validate_env_entry("K", "v\nEVIL=1").is_err());
    }

    /// 意图:acme.env 覆盖式写入,unix 权限 0600(C3)。
    #[test]
    fn write_env_file_creates_0600() {
        let dir = tmpdir("env");
        let p = dir.join("acme.env");
        let mut env = BTreeMap::new();
        env.insert("A_KEY".to_string(), "v1".to_string());
        env.insert("B_KEY".to_string(), "sp ace".to_string());
        write_env_file(&p, &env).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text, "A_KEY=v1\nB_KEY=\"sp ace\"\n");
        // 覆盖式:旧键不残留
        let mut env2 = BTreeMap::new();
        env2.insert("C_KEY".to_string(), "v2".to_string());
        write_env_file(&p, &env2).unwrap();
        assert!(!std::fs::read_to_string(&p).unwrap().contains("A_KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "凭据文件须 0600");
            // 预存在且权限过宽(0644,如手工 touch 过)→ 覆盖写后收敛回 0600:
            // OpenOptions::mode 仅创建时生效,不显式 chmod 凭据会落成可读
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_env_file(&p, &env2).unwrap();
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600,
                "宽权限预存文件须被收敛为 0600"
            );
        }
        // 非法键在写盘前被拒
        let mut bad = BTreeMap::new();
        bad.insert("9bad".to_string(), "v".to_string());
        assert!(write_env_file(&p, &bad).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图:resolve_lego 跳过不存在的候选,对存在的候选跑 --version 命中
    /// 并解析版本;无候选(空 PATH)→ 未安装。
    #[tokio::test]
    async fn resolve_lego_finds_candidate_and_parses_version() {
        let dir = tmpdir("resolve");
        let fake = dir.join("lego");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        let runner = FakeRunner {
            exit: 0,
            lines: vec!["lego version v5.6.0 linux/amd64"],
        };
        let st = resolve_lego_with(
            &runner,
            Some(fake.to_str().unwrap()),
            &dir,
            Some(std::ffi::OsString::new()),
        )
        .await;
        assert!(st.installed);
        assert_eq!(st.version.as_deref(), Some("5.6.0"), "v 前缀应剥掉");
        assert_eq!(st.source, Some(LegoSource::Explicit));

        let none = resolve_lego_with(&runner, None, &dir, Some(std::ffi::OsString::new())).await;
        assert!(!none.installed, "无候选应未安装");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图:版本行解析——真 lego(无 v 前缀)与垫片(v 前缀)都取纯版本号;
    /// 无匹配回退首个数字 token / 整行。
    #[test]
    fn parse_lego_version_token() {
        assert_eq!(
            parse_lego_version("lego version 5.5.2 linux/amd64"),
            "5.5.2"
        );
        assert_eq!(
            parse_lego_version("lego version v5.6.0 linux/amd64"),
            "5.6.0"
        );
        assert_eq!(parse_lego_version("lego version v0.0.0-fake"), "0.0.0-fake");
        assert_eq!(parse_lego_version("5.6.0"), "5.6.0");
        assert_eq!(
            parse_lego_version("lego unknown output"),
            "lego unknown output"
        );
    }

    /// 意图:env_file 解析——显式优先;缺省落配置文件目录(C3 修订版)。
    #[test]
    fn resolve_env_file_default_is_config_dir() {
        let mut acme = AcmeConfig::default();
        assert_eq!(
            resolve_env_file(&acme, Path::new("/w/config/services.toml")),
            PathBuf::from("/w/config/acme.env")
        );
        acme.env_file = Some("/etc/acme.env".into());
        assert_eq!(
            resolve_env_file(&acme, Path::new("/w/config/services.toml")),
            PathBuf::from("/etc/acme.env")
        );
    }

    /// 意图:真实执行器——流式行捕获(stdout+stderr)、退出码传递、
    /// 不存在程序报 spawn 失败。(unix 下以 /bin/sh 自测;Windows 由
    /// e2e fake-lego 覆盖)
    #[cfg(unix)]
    #[tokio::test]
    async fn process_runner_streams_lines_and_exit_code() {
        use std::sync::Mutex as StdMutex;
        let runner = ProcessRunner {
            timeout: Duration::from_secs(10),
        };
        let got = Arc::new(StdMutex::new(Vec::new()));
        let sink = sink_to_vec(&got);
        let out = runner
            .run(
                Path::new("/bin/sh"),
                &[
                    "-c".to_string(),
                    "echo out1; echo err1 >&2; exit 3".to_string(),
                ],
                &sink,
            )
            .await;
        assert!(!out.success);
        assert_eq!(out.exit, Some(3));
        let lines = lock(got.as_ref()).clone();
        assert!(lines.contains(&"out1".to_string()), "{lines:?}");
        assert!(lines.contains(&"err1".to_string()), "stderr 也须进面板");

        let out2 = runner.run(Path::new("/no/such/binary"), &[], &sink).await;
        assert!(!out2.success);
        assert!(out2.spawn_error.is_some(), "spawn 失败须可区分:{out2:?}");
    }

    /// 意图(F4):编排 future panic 不得把任务留在 Running——否则无人 await 的
    /// spawn 静默吞掉 panic,所有证书端点 409 锁死到重启;RunningGuard 须在
    /// unwind 时强制置失败。占位候选文件让 resolve_lego 必然调用 runner(panic 点)。
    #[tokio::test]
    async fn task_panic_marks_failed_not_stuck_running() {
        struct PanicRunner;
        #[async_trait]
        impl LegoRunner for PanicRunner {
            async fn run(&self, _p: &Path, _a: &[String], _s: &LineSink) -> ExecOutcome {
                panic!("boom");
            }
        }
        let dir = tmpdir("panic");
        let fake = dir.join("lego");
        std::fs::write(&fake, "").unwrap();
        let mgr = Arc::new(CertMgr::new());
        mgr.tasks().try_begin(TaskKind::Issue).unwrap();
        let acme = AcmeConfig {
            lego_path: Some(fake.display().to_string()),
            ..AcmeConfig::default()
        };
        let ctx = OrchestratorCtx {
            data_dir: dir.clone(),
            config_path: dir.join("services.toml"),
            acme,
            cert_file: None,
            key_file: None,
            domain: Some("x.test".into()),
            edit_lock: Arc::new(Mutex::new(())),
        };
        let h = tokio::spawn(execute_issue(
            mgr.clone(),
            Arc::new(PanicRunner),
            ctx,
            IssueParams {
                email: "ops@t.dev".into(),
                dns_provider: "fakedns".into(),
                server: None,
                domains: None,
                env: BTreeMap::new(),
                persist: false,
            },
        ));
        let joined = h.await;
        assert!(
            joined.is_err(),
            "runner panic 应传播为 JoinError:{joined:?}"
        );
        assert!(
            !mgr.tasks().is_running(),
            "panic 后不得停在 Running(409 锁死)"
        );
        let snap = mgr.tasks().snapshot(10).unwrap();
        assert_eq!(snap.status, "failed");
        assert!(
            snap.message
                .as_deref()
                .is_some_and(|m| m.contains("异常终止")),
            "{snap:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
