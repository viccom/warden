//! 配置加载:toml 解析 + Default + 路径查找 + 校验(坏项跳过)。
//!
//! 路径查找优先级(对齐 serviceMgr-tui):
//!   1. `$WARDEN_CONFIG`
//!   2. `<exe_dir>/config/services.toml`
//!   3. `./config/services.toml`
//!   4. 平台标准位置(用户级 config dir)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::WardenError;
use crate::model::ServiceConfig;

/// 顶层配置。
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default, rename = "service")]
    pub services: Vec<ServiceConfig>,
    /// 反向代理段(存在 = 启用;feature 未编译时仅启动 warn 并忽略)。
    /// 无条件解析(D8:配置层无 cfg 分叉,双 feature 形态解析行为一致)。
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
}

/// daemon 自身配置。
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct DaemonConfig {
    #[serde(default = "default_api_bind")]
    pub api_bind: String,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
    /// 健康检查告警 webhook(状态迁移时 POST JSON;空则仅日志)。
    #[serde(default)]
    pub alert_webhook: Option<String>,
    /// 全局环境变量:注入所有被监护进程(service 同名 key 覆盖全局)。
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 窗口/标题栏名称(桌面版消费:标题栏与任务栏标题;CLI 忽略)。空 = 默认名。
    #[serde(default)]
    pub title: Option<String>,
}

fn default_api_bind() -> String {
    "127.0.0.1:8789".into()
}
fn default_data_dir() -> String {
    "./data".into()
}
fn default_log_dir() -> String {
    "./logs".into()
}

/// 默认配置文件名(查找链与 CRUD 兜底创建共用;桌面版另用 services.desktop.toml)。
pub const CONFIG_FILE_NAME: &str = "services.toml";

/// 未找到任何配置文件时,CRUD 首次写回的默认创建路径(cwd 下,与查找链第 3 级
/// 一致,后续启动可被重新找到)。
pub fn default_config_create_path() -> PathBuf {
    PathBuf::from("config").join(CONFIG_FILE_NAME)
}

/// 配置基准目录(配置内相对路径的锚点):
/// - 文件不存在 → None(空配置起步不锚定)
/// - 文件在 `<base>/config/` 下 → `<base>`(查找链布局:`../xxx` 相对 base)
/// - 其他位置(如 `$WARDEN_CONFIG` 任意路径) → 文件所在目录
///
/// daemon 启动时把进程 cwd 锚定到这里(`run_app_with_shutdown`/桌面版
/// `daemon::start`):配置内相对 working_dir/command/data_dir/log_dir 与
/// 启动方式(启动目录/Service 的 System32 cwd)解耦,配置随目录整体迁移。
pub fn config_base_dir(cfg_path: &Path) -> Option<PathBuf> {
    if !cfg_path.exists() {
        return None;
    }
    let parent = cfg_path.parent()?;
    if parent.file_name().is_some_and(|n| n == "config") {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(parent.to_path_buf())
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            api_bind: default_api_bind(),
            auth_token: String::new(),
            data_dir: default_data_dir(),
            log_dir: default_log_dir(),
            alert_webhook: None,
            env: HashMap::new(),
            title: None,
        }
    }
}

// ── 反向代理配置([proxy] 段,无条件解析;引擎在 reverse-proxy feature 下)──

/// 反向代理全局配置。设计见 docs/PLAN-REVERSE-PROXY.md §4.1。
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProxyConfig {
    /// 根级域名:auto 路由 = <subdomain>.<domain>。
    #[serde(default)]
    pub domain: Option<String>,
    /// HTTP 监听地址;缺省/空 = 不监听。
    #[serde(default)]
    pub http_bind: Option<String>,
    /// HTTPS 监听地址(P2 起;P0/P1 解析但不消费)。
    #[serde(default)]
    pub https_bind: Option<String>,
    /// 上游连接超时(仅约束连接建立,不约束请求/响应全程——SSE/大文件不应被误杀)。
    #[serde(default = "default_proxy_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// 全局缺省:转发时 Host 重写为上游;true = 保留客户端 Host。
    #[serde(default)]
    pub preserve_host: bool,
    /// 显式路由表(与 auto 并存时显式优先)。
    #[serde(default, rename = "route")]
    pub routes: Vec<ProxyRoute>,
}

fn default_proxy_connect_timeout_ms() -> u64 {
    5000
}

/// 单条显式路由:精确 host 或 `*.` 单层通配;`to`/`service` 二选一。
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProxyRoute {
    /// 精确 host(如 `fs2.opc.dongx.site`)或单层通配(`*.opc.dongx.site`)。
    pub host: String,
    /// 显式上游(http/https URI)。
    #[serde(default)]
    pub to: Option<String>,
    /// 引用被监护服务名(上游 = 该服务 ui_url,请求时经 snapshot 解析)。
    #[serde(default)]
    pub service: Option<String>,
    /// 路由级 Host 保留覆盖(None = 用全局 preserve_host)。
    #[serde(default)]
    pub preserve_host: Option<bool>,
}

/// 规范化 [proxy] 段:domain 与路由 host 小写化存储(匹配层统一小写比较,幂等);
/// 服务的 subdomain 同步小写化(校验按小写判断,存储须对齐——引擎把请求 Host
/// 小写后与标签比对,原样存大写会静默 miss)。
pub fn normalize_config(mut cfg: Config) -> Config {
    if let Some(p) = &mut cfg.proxy {
        p.domain = p.domain.take().map(|d| d.to_lowercase());
        for r in &mut p.routes {
            r.host = r.host.to_lowercase();
        }
    }
    for svc in &mut cfg.services {
        if let Some(sd) = &mut svc.subdomain {
            *sd = sd.to_lowercase();
        }
    }
    cfg
}

/// 校验 [proxy] 段(服务引用按 cfg 自身服务名表)。
pub fn validate_config(cfg: &Config) -> Vec<String> {
    let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
    validate_config_with_services(cfg, &names)
}

/// 校验 [proxy] 段(纯函数,坏项 warn 不致命——返回告警文案供调用方记日志,
/// 路由保留不剔除,引擎侧自行容忍)。校验清单见设计 §4.1。
pub fn validate_config_with_services(cfg: &Config, service_names: &[&str]) -> Vec<String> {
    let Some(p) = &cfg.proxy else {
        return Vec::new();
    };
    let mut warns = Vec::new();
    let bind_empty = |b: &Option<String>| b.as_deref().map_or(true, |s| s.is_empty());
    if bind_empty(&p.http_bind) && bind_empty(&p.https_bind) {
        warns.push("[proxy] http_bind 与 https_bind 均为空,反代无监听地址".into());
    }
    if let Some(d) = p.domain.as_deref() {
        if d.is_empty() || d.contains(['/', '\\', ':', '*']) {
            warns.push(format!(
                "[proxy] domain '{d}' 非法(须为纯域名,无 path/端口/通配符;auto 路由整体不可用)"
            ));
        }
    }
    let mut seen = HashSet::new();
    for r in &p.routes {
        if r.host.is_empty() {
            warns.push("路由 host 为空(须为域名或 '*.' 通配)".into());
        } else if r.host.contains('/') || r.host.contains('\\') {
            warns.push(format!("路由 '{}':host 禁 path 部分", r.host));
        } else if r.host.contains(':') {
            warns.push(format!(
                "路由 '{}':host 禁端口(请求 Host 匹配前已剥端口,带端口的 host 永不命中)",
                r.host
            ));
        } else if r.host.contains('*') && !r.host.starts_with("*.") {
            warns.push(format!("路由 '{}':通配仅支持 '*.' 前缀形态(单层)", r.host));
        }
        // to / service 二选一且必填其一
        match (&r.to, &r.service) {
            (Some(_), Some(_)) => {
                warns.push(format!("路由 '{}':to 与 service 二选一(同时配置)", r.host));
            }
            (None, None) => {
                warns.push(format!("路由 '{}':to 与 service 二选一(均未配置)", r.host));
            }
            _ => {}
        }
        if let Some(to) = &r.to {
            if !valid_upstream_uri(to) {
                warns.push(format!(
                    "路由 '{}':to 必须是合法的 http/https URI:{to}",
                    r.host
                ));
            }
        }
        // 通配仅允许前缀 `*.` 且只匹配单层子域:单根域 + 单张通配证书模型(D9/D10)
        // 下,配置层只认 `*.<domain>`;引擎匹配更通用(最长后缀),不在此限制
        if let Some(suffix) = r.host.strip_prefix("*.") {
            match &p.domain {
                Some(d) if suffix == d => {}
                Some(d) => warns.push(format!(
                    "路由 '{}':通配仅支持单层子域 '*.{d}'(与 [proxy] domain 对齐)",
                    r.host
                )),
                None => warns.push(format!("路由 '{}':通配路由需要 [proxy] domain", r.host)),
            }
        }
        if let Some(svc) = &r.service {
            if !service_names.contains(&svc.as_str()) {
                warns.push(format!("路由 '{}' 引用的服务 '{svc}' 不存在", r.host));
            }
        }
        if !seen.insert(r.host.clone()) {
            warns.push(format!("路由 host '{}' 重复", r.host));
        }
    }
    // auto 暴露的服务检查(domain 未配时 auto 整体不可用,不产生误导性告警)
    if p.domain.is_some() {
        for svc in &cfg.services {
            if !svc.proxy {
                continue;
            }
            if svc.ui_url.as_deref().map_or(true, |u| u.is_empty()) {
                warns.push(format!(
                    "服务 '{}':proxy = true 但未配置 ui_url,不进 auto 路由",
                    svc.name
                ));
            }
            let label = svc
                .subdomain
                .clone()
                .unwrap_or_else(|| svc.name.clone())
                .to_lowercase();
            if label.is_empty()
                || !label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                warns.push(format!(
                    "服务 '{}':auto 路由标签 '{label}' 不满足 [a-z0-9]+,不暴露",
                    svc.name
                ));
            }
        }
    }
    warns
}

/// 上游 URI 粗校验:必须 http/https 绝对地址且 authority 非空(warn 级,
/// 转发层在 P1 会再按 URI 正式解析)。
fn valid_upstream_uri(s: &str) -> bool {
    match s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    {
        Some(auth) => !auth.is_empty() && !auth.contains(' ') && !auth.starts_with('/'),
        None => false,
    }
}

/// 服务名禁用字符(文件系统 / 路径 / Windows 服务名不安全)。
const NAME_FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// 校验单个服务配置(运行时 CRUD 复用)。返回 Err(msg) 的项会被跳过。
pub fn validate_service(svc: &ServiceConfig, seen: &mut HashSet<String>) -> Result<(), String> {
    if svc.name.trim().is_empty() {
        return Err("name 为空".into());
    }
    if svc.name.chars().any(|c| NAME_FORBIDDEN.contains(&c)) {
        return Err(format!(
            "name '{}' 含禁用字符(禁用 / \\ : * ? \" < > |)",
            svc.name
        ));
    }
    if !seen.insert(svc.name.clone()) {
        return Err(format!("name '{}' 重复", svc.name));
    }
    if svc.command.trim().is_empty() {
        return Err(format!("service '{}' 的 command 为空", svc.name));
    }
    // 组名用于组级 API 路径参数(/api/v1/groups/{group}/start),含 '/' 会破坏路由
    if let Some(g) = &svc.group {
        if g.contains('/') {
            return Err(format!(
                "service '{}' 的 group '{g}' 含禁用字符 '/'",
                svc.name
            ));
        }
    }
    Ok(())
}

impl Config {
    /// 解析 toml 字符串 + 规范化 + 校验(坏项跳过并 warn,不致命)。
    pub fn parse(s: &str) -> Result<Self, WardenError> {
        let mut cfg: Config = normalize_config(toml::from_str(s)?);
        cfg.validate();
        Ok(cfg)
    }

    /// 校验:剔除坏服务项(warn),保留有效项;[proxy] 坏项 warn 但保留。
    fn validate(&mut self) {
        let mut seen = HashSet::new();
        let original = std::mem::take(&mut self.services);
        for svc in original {
            match validate_service(&svc, &mut seen) {
                Ok(()) => self.services.push(svc),
                Err(e) => tracing::warn!("[config] 跳过无效服务项:{e}"),
            }
        }
        for e in validate_config(self) {
            tracing::warn!("[config] [proxy] {e}");
        }
    }

    /// 按路径查找规则定位配置文件,返回首个存在的路径。
    pub fn find_config_path() -> Option<PathBuf> {
        // 1. $WARDEN_CONFIG
        if let Ok(p) = std::env::var("WARDEN_CONFIG") {
            let p = PathBuf::from(p);
            if p.exists() {
                return Some(p);
            }
        }
        // 2. <exe_dir>/config/services.toml
        if let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            let p = exe.join("config").join(CONFIG_FILE_NAME);
            if p.exists() {
                return Some(p);
            }
        }
        // 3. ./config/services.toml
        let p = default_config_create_path();
        if p.exists() {
            return Some(p);
        }
        // 4. 平台标准位置(用户级)
        if let Some(proj) = directories::ProjectDirs::from("io", "warden", "warden") {
            let p = proj.config_dir().join(CONFIG_FILE_NAME);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    /// 加载:指定路径优先(必须存在),否则按规则查找。
    pub fn load(path: Option<&Path>) -> Result<Self, WardenError> {
        let resolved = match path {
            Some(p) if p.exists() => p.to_path_buf(),
            Some(p) => {
                return Err(WardenError::Config(format!(
                    "配置文件不存在:{}",
                    p.display()
                )))
            }
            None => Self::find_config_path().ok_or_else(|| {
                WardenError::Config(
                    "未找到配置文件(已尝试 $WARDEN_CONFIG / exe_dir / cwd / 平台标准位置)".into(),
                )
            })?,
        };
        let s = std::fs::read_to_string(&resolved)
            .map_err(|e| WardenError::Config(format!("读取 {}: {e}", resolved.display())))?;
        tracing::info!("[config] 加载 {}", resolved.display());
        Self::parse(&s)
    }

    /// 把 `[daemon] env` 全局环境变量烘入每个服务的 environment:
    /// 仅插入服务未定义的 key(service 同名 key 覆盖全局,经典 supervisor 语义)。
    /// 烘入后 spawn/API/序列化全链路自然生效;操作幂等。
    pub fn apply_daemon_env(&mut self) {
        if self.daemon.env.is_empty() {
            return;
        }
        for svc in &mut self.services {
            for (k, v) in &self.daemon.env {
                svc.environment
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_empty() {
        let cfg = Config::default();
        assert!(cfg.services.is_empty());
        assert_eq!(cfg.daemon.api_bind, "127.0.0.1:8789");
        assert_eq!(cfg.daemon.auth_token, "");
    }

    #[test]
    fn parse_minimal_service_uses_defaults() {
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.services.len(), 1);
        let s = &cfg.services[0];
        assert_eq!(s.name, "a");
        assert!(!s.auto_start); // 默认 false
        assert!(!s.auto_restart); // 默认 false
        assert_eq!(s.restart.max_retries, 3); // 默认 RestartPolicy
        assert_eq!(s.restart.backoff_initial_ms, 1000);
        assert!(s.health.is_none());
    }

    #[test]
    fn skips_empty_name_and_duplicate() {
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"

[[service]]
name = ""
command = "/bin/false"

[[service]]
name = "a"
command = "/bin/echo"
"#;
        let cfg = Config::parse(toml).unwrap();
        // a 有效;空 name 跳过;重复 a 跳过
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "a");
        assert_eq!(cfg.services[0].command, "/bin/true");
    }

    #[test]
    fn skips_name_with_forbidden_chars() {
        let toml = r#"
[[service]]
name = "a/b"
command = "/bin/true"

[[service]]
name = "c"
command = "/bin/true"
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "c");
    }

    #[test]
    fn restart_policy_override() {
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"
restart = { max_retries = 5, backoff_initial_ms = 500, backoff_max_ms = 30000, backoff_factor = 3.0, restart_window_secs = 120 }
"#;
        let cfg = Config::parse(toml).unwrap();
        let r = &cfg.services[0].restart;
        assert_eq!(r.max_retries, 5);
        assert_eq!(r.backoff_factor, 3.0);
        assert_eq!(r.backoff_max_ms, 30000);
    }

    /// 意图:RestartPolicy 全字段解析(mode / expected_exit_codes /
    /// max_retries / 退避参数)直读正确,配套子进程自升级场景。
    /// 缺这些字段的旧配置 → 走 Default 兜底,单测已由 `restart_policy_override`
    /// 默认值侧覆盖(mode 缺省 Always / expected_exit_codes 缺省 vec![0])。
    #[test]
    fn restart_policy_with_unexpected_mode_parses() {
        // 多行 inline table 见 SERVICESAMPLE 转储(existing 风格)。toml 0.8
        // 不支持 inline table 跨行——必须单行。
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"
auto_restart = true
restart = { mode = "unexpected", expected_exit_codes = [0, 130], max_retries = 5, backoff_initial_ms = 500, backoff_max_ms = 30000, backoff_factor = 3.0, restart_window_secs = 120 }
"#;
        let cfg = Config::parse(toml).unwrap();
        let r = &cfg.services[0].restart;
        assert_eq!(r.mode, crate::model::RestartMode::Unexpected);
        assert_eq!(r.expected_exit_codes, vec![0, 130]);
        assert_eq!(r.max_retries, 5);
        assert_eq!(r.backoff_factor, 3.0);
    }

    #[test]
    fn daemon_section_defaults_when_absent() {
        let toml = "[[service]]\nname=\"a\"\ncommand=\"/bin/true\"\n";
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.daemon.api_bind, "127.0.0.1:8789");
        assert_eq!(cfg.daemon.data_dir, "./data");
    }

    #[test]
    fn daemon_section_explicit() {
        let toml = r#"
[daemon]
api_bind = "0.0.0.0:9999"
auth_token = "secret"
data_dir = "/var/warden"
log_dir = "/var/log/warden"

[[service]]
name = "a"
command = "/bin/true"
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.daemon.api_bind, "0.0.0.0:9999");
        assert_eq!(cfg.daemon.auth_token, "secret");
        assert_eq!(cfg.daemon.data_dir, "/var/warden");
    }

    #[test]
    fn invalid_toml_returns_error() {
        let res = Config::parse("this is not = = valid toml [[[[");
        assert!(res.is_err());
    }

    #[test]
    fn daemon_title_parse_and_default() {
        // 缺省 None(桌面版前端回退默认名;CLI 不消费此字段)
        let cfg = Config::parse("[daemon]\napi_bind = \"127.0.0.1:0\"\n").unwrap();
        assert!(cfg.daemon.title.is_none());
        let cfg =
            Config::parse("[daemon]\napi_bind = \"127.0.0.1:0\"\ntitle = \"rs-iot 现场监护\"\n")
                .unwrap();
        assert_eq!(cfg.daemon.title.as_deref(), Some("rs-iot 现场监护"));
    }

    /// 意图:配置内相对路径必须锚定配置所在基准目录,与启动 cwd 无关——
    /// config 子目录剥一层(`<base>/config/x.toml` → `<base>`,查找链布局)、
    /// 平级文件取所在目录($WARDEN_CONFIG 任意位置)、不存在不锚定、嵌套只剥一层。
    #[test]
    fn config_base_dir_anchors_relative_paths() {
        let tmp = std::env::temp_dir().join(format!("warden-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let touch = |p: std::path::PathBuf| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        };

        // 文件不存在 → None(无配置不锚定)
        assert_eq!(
            config_base_dir(&tmp.join("nope").join("services.toml")),
            None
        );

        // `<base>/config/x.toml` → `<base>`(查找链布局:exe_dir / cwd / 平台位置)
        let f = touch(
            tmp.join("deploy")
                .join("warden")
                .join("config")
                .join(CONFIG_FILE_NAME),
        );
        assert_eq!(config_base_dir(&f), Some(tmp.join("deploy").join("warden")));

        // 平级文件($WARDEN_CONFIG 任意路径) → 文件所在目录
        let f = touch(tmp.join("anywhere").join("my.toml"));
        assert_eq!(config_base_dir(&f), Some(tmp.join("anywhere")));

        // 嵌套 config 只剥一层:`<x>/config/config/x.toml` → `<x>/config`
        let f = touch(
            tmp.join("x")
                .join("config")
                .join("config")
                .join(CONFIG_FILE_NAME),
        );
        assert_eq!(config_base_dir(&f), Some(tmp.join("x").join("config")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn daemon_env_injects_and_service_overrides() {
        let toml = r#"
[daemon]
env = { LANG = "zh_CN", WARDEN = "1", OVERRIDE = "global" }

[[service]]
name = "a"
command = "/bin/true"
environment = { OVERRIDE = "service", OWN = "x" }
"#;
        let mut cfg = Config::parse(toml).unwrap();
        cfg.apply_daemon_env();
        let env = &cfg.services[0].environment;
        assert_eq!(
            env.get("LANG").map(String::as_str),
            Some("zh_CN"),
            "全局 env 应注入"
        );
        assert_eq!(env.get("WARDEN").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("OVERRIDE").map(String::as_str),
            Some("service"),
            "service 同名 key 覆盖全局"
        );
        assert_eq!(
            env.get("OWN").map(String::as_str),
            Some("x"),
            "service 自身 env 保留"
        );
        // 幂等:重复烘入不改变结果
        cfg.apply_daemon_env();
        assert_eq!(
            cfg.services[0]
                .environment
                .get("OVERRIDE")
                .map(String::as_str),
            Some("service")
        );
    }

    #[test]
    fn alert_webhook_default_none_and_parse() {
        let cfg = Config::parse("[[service]]\nname=\"a\"\ncommand=\"/bin/true\"\n").unwrap();
        assert!(cfg.daemon.alert_webhook.is_none());
        let cfg = Config::parse(
            "[daemon]\nalert_webhook=\"http://127.0.0.1:9/hook\"\n[[service]]\nname=\"a\"\ncommand=\"/bin/true\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.daemon.alert_webhook.as_deref(),
            Some("http://127.0.0.1:9/hook")
        );
    }

    #[test]
    fn parse_group_and_priority() {
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"
group = "core"
priority = 10
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.services[0].group.as_deref(), Some("core"));
        assert_eq!(cfg.services[0].priority, 10);
    }

    #[test]
    fn default_group_and_priority() {
        let toml = "[[service]]\nname=\"a\"\ncommand=\"/bin/true\"\n";
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.services[0].group, None, "group 缺省应为无分组");
        assert_eq!(cfg.services[0].priority, 0, "priority 缺省应为 0(最先启动)");
    }

    #[test]
    fn serialize_roundtrip_group_priority() {
        // runtime overlay(persist_runtime)直接序列化 ServiceConfig,
        // round-trip 不丢字段是 CRUD 重启恢复不丢配置的行为保障
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"
group = "edge"
priority = 7
ui_url = "http://127.0.0.1:8790"
config_file = "E:/rsiot-field/config.toml"
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(
            cfg.services[0].config_file.as_deref(),
            Some("E:/rsiot-field/config.toml")
        );
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.services[0].group.as_deref(), Some("edge"));
        assert_eq!(back.services[0].priority, 7);
        assert_eq!(
            back.services[0].ui_url.as_deref(),
            Some("http://127.0.0.1:8790")
        );
        assert_eq!(
            back.services[0].config_file.as_deref(),
            Some("E:/rsiot-field/config.toml")
        );
    }

    #[test]
    fn ui_url_and_config_file_default_none() {
        let toml = "[[service]]\nname=\"a\"\ncommand=\"/bin/true\"\n";
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.services[0].ui_url, None);
        assert_eq!(
            cfg.services[0].config_file, None,
            "config_file 缺省应为 None"
        );
    }

    #[test]
    fn skips_group_with_path_separator() {
        // 组名用于组级 API 路径参数(/api/v1/groups/{group}/start),含 '/' 会破坏路由
        let toml = r#"
[[service]]
name = "a"
command = "/bin/true"
group = "web/ui"

[[service]]
name = "b"
command = "/bin/true"
group = "web"
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(cfg.services.len(), 1, "含 '/' 的组名应整项跳过");
        assert_eq!(cfg.services[0].name, "b");
    }

    #[test]
    fn parse_proxy_section() {
        let toml = r#"
[proxy]
domain = "opc.dongx.site"
http_bind = "0.0.0.0:8080"
connect_timeout_ms = 3000
preserve_host = true

[[proxy.route]]
host = "fs2.opc.dongx.site"
to = "http://127.0.0.1:9000"
"#;
        let cfg = Config::parse(toml).unwrap();
        let p = cfg.proxy.expect("proxy 段存在");
        assert_eq!(p.domain.as_deref(), Some("opc.dongx.site"));
        assert_eq!(p.http_bind.as_deref(), Some("0.0.0.0:8080"));
        assert_eq!(p.https_bind, None);
        assert_eq!(p.connect_timeout_ms, 3000);
        assert!(p.preserve_host);
        assert_eq!(p.routes.len(), 1);
        let r = &p.routes[0];
        assert_eq!(r.host, "fs2.opc.dongx.site");
        assert_eq!(r.to.as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(r.service, None);
        assert!(!r.preserve_host.unwrap_or(false));
    }

    #[test]
    fn default_proxy_absent() {
        let cfg = Config::parse("[daemon]\napi_bind = \"127.0.0.1:8789\"\n").unwrap();
        assert!(cfg.proxy.is_none(), "无 [proxy] 段 → None");
    }

    #[test]
    fn proxy_defaults_when_partial() {
        let toml = "[proxy]\ndomain = \"x.example.com\"\n";
        let cfg = Config::parse(toml).unwrap();
        let p = cfg.proxy.unwrap();
        assert_eq!(p.connect_timeout_ms, 5000, "缺省 5000");
        assert!(!p.preserve_host, "缺省 false");
        assert!(p.http_bind.is_none() && p.https_bind.is_none());
    }

    #[test]
    fn validate_proxy_rejects_to_and_service_both() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[proxy.route]]
host = "a.x.example.com"
to = "http://1"
service = "svc"
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(errs
            .iter()
            .any(|e| e.contains("to 与 service") && e.contains("二选一")));
    }

    #[test]
    fn validate_proxy_normalizes_host_lowercase() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[proxy.route]]
host = "A.X.Example.COM"
to = "http://1"
"#;
        let cfg = Config::parse(toml).unwrap();
        let cfg = normalize_config(cfg);
        assert_eq!(cfg.proxy.unwrap().routes[0].host, "a.x.example.com");
    }

    #[test]
    fn validate_proxy_rejects_wildcard_multilevel() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[proxy.route]]
host = "*.b.x.example.com"
to = "http://1"
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(errs.iter().any(|e| e.contains("仅支持单层")));
    }

    #[test]
    fn validate_proxy_rejects_duplicate_host() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[proxy.route]]
host = "a.x.example.com"
to = "http://1"
[[proxy.route]]
host = "a.x.example.com"
to = "http://2"
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(errs.iter().any(|e| e.contains("重复")));
    }

    #[test]
    fn validate_proxy_warns_when_both_binds_empty() {
        let toml = "[proxy]\ndomain = \"x.example.com\"\n";
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(errs.iter().any(|e| e.contains("无监听")));
    }

    #[test]
    fn validate_proxy_warns_service_not_found() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[proxy.route]]
host = "a.x.example.com"
service = "ghost"
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config_with_services(&r, &["other"]);
        assert!(errs
            .iter()
            .any(|e| e.contains("ghost") && e.contains("不存在")));
    }

    #[test]
    fn validate_proxy_warns_service_without_ui_url() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[service]]
name = "fs"
command = "/bin/true"
proxy = true
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(errs
            .iter()
            .any(|e| e.contains("fs") && e.contains("ui_url")));
    }

    #[test]
    fn validate_proxy_warns_invalid_subdomain_label() {
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[service]]
name = "fs"
command = "/bin/true"
proxy = true
ui_url = "http://127.0.0.1:8790"
subdomain = "fs-2"
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(errs
            .iter()
            .any(|e| e.contains("fs-2") && e.contains("[a-z0-9]+")));
    }

    #[test]
    fn validate_proxy_accepts_uppercase_subdomain_after_lowering() {
        // 标签校验按小写化后判断:FS2 → fs2 合法,不告警
        let toml = r#"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
[[service]]
name = "fs"
command = "/bin/true"
proxy = true
ui_url = "http://127.0.0.1:8790"
subdomain = "FS2"
"#;
        let r = Config::parse(toml).unwrap();
        let errs = validate_config(&r);
        assert!(!errs.iter().any(|e| e.contains("[a-z0-9]+")));
    }

    /// 意图:subdomain 小写化存储(校验层按小写判断,存储层须对齐)——
    /// P1 引擎把请求 Host 小写化后与标签比对,原样存大写会静默 miss → 421。
    #[test]
    fn normalize_lowercases_subdomain_storage() {
        let toml = r#"
[[service]]
name = "fs"
command = "/bin/true"
proxy = true
subdomain = "FS2"
"#;
        let cfg = Config::parse(toml).unwrap();
        assert_eq!(
            cfg.services[0].subdomain.as_deref(),
            Some("fs2"),
            "subdomain 须小写化存储"
        );
    }

    /// 意图:格式非法的 host(空串/裸 */含端口)永不匹配任何请求 Host
    /// (§4.3 小写+剥端口后比对),必须 warn 给用户反馈,不留静默脏路由。
    #[test]
    fn validate_proxy_warns_malformed_hosts() {
        let base = "[proxy]\ndomain = \"x.example.com\"\nhttp_bind = \"0.0.0.0:8080\"\n";
        for (host, expect) in [
            ("", "host 为空"),
            ("*", "'*.' 前缀"),
            ("a*.x.example.com", "'*.' 前缀"),
            ("a.x.example.com:8080", "禁端口"),
        ] {
            let toml = format!("{base}[[proxy.route]]\nhost = \"{host}\"\nto = \"http://1\"\n");
            let r = Config::parse(&toml).unwrap();
            let errs = validate_config(&r);
            assert!(
                errs.iter().any(|e| e.contains(expect)),
                "host '{host}' 应产生含 '{expect}' 的告警,实际:{errs:?}"
            );
        }
    }

    /// 意图:domain 空串/含 path/端口/通配符时 auto 路由整体不可用或产生
    /// 怪异剥后缀匹配,必须 warn。
    #[test]
    fn validate_proxy_warns_invalid_domain() {
        for domain in [
            "",
            "x.example.com:443",
            "a/b.example.com",
            "*.x.example.com",
        ] {
            let toml = format!("[proxy]\ndomain = \"{domain}\"\nhttp_bind = \"0.0.0.0:8080\"\n");
            let r = Config::parse(&toml).unwrap();
            let errs = validate_config(&r);
            assert!(
                errs.iter()
                    .any(|e| e.contains("domain") && e.contains("非法")),
                "domain '{domain}' 应告警非法,实际:{errs:?}"
            );
        }
    }
}
