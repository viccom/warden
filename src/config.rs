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

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            api_bind: default_api_bind(),
            auth_token: String::new(),
            data_dir: default_data_dir(),
            log_dir: default_log_dir(),
            alert_webhook: None,
            env: HashMap::new(),
        }
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
    Ok(())
}

impl Config {
    /// 解析 toml 字符串 + 校验(坏项跳过并 warn,不致命)。
    pub fn parse(s: &str) -> Result<Self, WardenError> {
        let mut cfg: Config = toml::from_str(s)?;
        cfg.validate();
        Ok(cfg)
    }

    /// 校验:剔除坏服务项(warn),保留有效项。
    fn validate(&mut self) {
        let mut seen = HashSet::new();
        let original = std::mem::take(&mut self.services);
        for svc in original {
            match validate_service(&svc, &mut seen) {
                Ok(()) => self.services.push(svc),
                Err(e) => tracing::warn!("[config] 跳过无效服务项:{e}"),
            }
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
            let p = exe.join("config").join("services.toml");
            if p.exists() {
                return Some(p);
            }
        }
        // 3. ./config/services.toml
        let p = PathBuf::from("config").join("services.toml");
        if p.exists() {
            return Some(p);
        }
        // 4. 平台标准位置(用户级)
        if let Some(proj) = directories::ProjectDirs::from("io", "warden", "warden") {
            let p = proj.config_dir().join("services.toml");
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
"#;
        let cfg = Config::parse(toml).unwrap();
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.services[0].group.as_deref(), Some("edge"));
        assert_eq!(back.services[0].priority, 7);
    }
}
