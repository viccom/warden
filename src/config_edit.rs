//! 配置文件编辑(CRUD 写回唯一数据源)。
//!
//! 用 `toml_edit` 做文档级编辑:未触及的服务条目(含注释/排版)原样保留,
//! 被编辑的条目以规范序列化替换。写入走「临时文件 + rename」原子替换。
//! 进程内并发由 `AppState::config_edit_lock` 保证单写者;每次操作重新读盘,
//! 外部手工编辑不会被内存副本覆盖。
//!
//! 另含旧双源文件的一次性迁移(见 `migrate_legacy_sources`)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, Document, Item, Table};

use crate::error::{WResult, WardenError};
use crate::model::ServiceConfig;

/// 首次创建配置文件时写入的说明头。
const FILE_HEADER: &str = "# warden 服务配置(由 warden 自动创建;本文件是服务定义的唯一数据源)\n\n";

/// 配置文件的内存编辑视图。
pub struct ConfigFile {
    doc: Document,
    path: PathBuf,
    /// 目标文件在 load 时不存在(首次创建,save 时写说明头)。
    fresh: bool,
}

impl ConfigFile {
    /// 加载已存在的配置文件(不存在则报错)。
    pub fn load(path: &Path) -> WResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| WardenError::Config(format!("读取 {} 失败:{e}", path.display())))?;
        Self::from_text(path, &text)
    }

    /// 加载;不存在则按空文档起步(save 时创建文件并写说明头)。
    pub fn load_or_create(path: &Path) -> WResult<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_text(path, &text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                doc: Document::new(),
                path: path.to_path_buf(),
                fresh: true,
            }),
            Err(e) => Err(WardenError::Config(format!(
                "读取 {} 失败:{e}",
                path.display()
            ))),
        }
    }

    fn from_text(path: &Path, text: &str) -> WResult<Self> {
        let doc: Document = text
            .parse()
            .map_err(|e| WardenError::Config(format!("解析 {} 失败:{e}", path.display())))?;
        Ok(Self {
            doc,
            path: path.to_path_buf(),
            fresh: false,
        })
    }

    /// 追加一个服务条目(不查重;调用方先 validate)。
    pub fn append_service(&mut self, svc: &ServiceConfig) -> WResult<()> {
        let arr = service_array_mut(&mut self.doc)?;
        arr.push(svc_to_table(svc));
        Ok(())
    }

    /// 按 name 原位替换服务条目(不存在报错)。
    pub fn replace_service(&mut self, svc: &ServiceConfig) -> WResult<()> {
        let arr = service_array_mut(&mut self.doc)?;
        let idx = find_index(arr, &svc.name).ok_or_else(|| {
            WardenError::Config(format!("配置文件中不存在服务 '{}' 的条目", svc.name))
        })?;
        *arr.get_mut(idx).expect("find_index 已校验") = svc_to_table(svc);
        Ok(())
    }

    /// 按 name 替换,不存在则追加(迁移合并用)。
    pub fn replace_or_append_service(&mut self, svc: &ServiceConfig) -> WResult<()> {
        let arr = service_array_mut(&mut self.doc)?;
        match find_index(arr, &svc.name) {
            Some(idx) => *arr.get_mut(idx).expect("find_index 已校验") = svc_to_table(svc),
            None => arr.push(svc_to_table(svc)),
        }
        Ok(())
    }

    /// 按 name 删除服务条目(不存在报错)。
    pub fn remove_service(&mut self, name: &str) -> WResult<()> {
        let arr = service_array_mut(&mut self.doc)?;
        let idx = find_index(arr, name)
            .ok_or_else(|| WardenError::Config(format!("配置文件中不存在服务 '{name}' 的条目")))?;
        arr.remove(idx);
        Ok(())
    }

    /// 按 name 设置 auto_start(条目不存在则忽略,迁移用)。
    pub fn set_auto_start(&mut self, name: &str, on: bool) {
        let Ok(arr) = service_array_mut(&mut self.doc) else {
            return;
        };
        if let Some(idx) = find_index(arr, name) {
            let t = arr.get_mut(idx).expect("find_index 已校验");
            t["auto_start"] = toml_edit::value(on);
        }
    }

    /// 落盘(原子写:临时文件 + rename)。
    pub fn save(&self) -> WResult<()> {
        let mut out = self.doc.to_string();
        if self.fresh {
            out = format!("{FILE_HEADER}{out}");
        }
        atomic_write(&self.path, &out)
    }
}

/// 取(或初始化)`[[service]]` 数组;service 段存在但非数组时报错。
fn service_array_mut(doc: &mut Document) -> WResult<&mut ArrayOfTables> {
    if !doc.contains_key("service") {
        doc.insert("service", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    doc["service"]
        .as_array_of_tables_mut()
        .ok_or_else(|| WardenError::Config("配置文件的 service 段不是 [[service]] 数组".into()))
}

/// 在 [[service]] 数组中按 name 定位下标。
fn find_index(arr: &ArrayOfTables, name: &str) -> Option<usize> {
    arr.iter()
        .position(|t| t.get("name").and_then(Item::as_str) == Some(name))
}

/// 单个服务配置构造为扁平 [[service]] 元素表:子结构(environment/restart/health)
/// 用内联表。不走去字符串 round-trip——经 Document 解析的表带位置/修饰元数据,
/// 塞进 ArrayOfTables 后子表头渲染错位,会产出非法 TOML(单测实证)。
/// Option 字段 None 时省略键(与 toml 序列化行为一致,round-trip 等价)。
fn svc_to_table(svc: &ServiceConfig) -> Table {
    use toml_edit::{value, Value};
    let mut t = Table::new();
    t["name"] = value(svc.name.clone());
    t["display_name"] = value(svc.display_name.clone());
    t["description"] = value(svc.description.clone());
    t["command"] = value(svc.command.clone());
    let args: toml_edit::Array = svc.args.iter().map(Value::from).collect();
    t["args"] = value(args);
    if let Some(v) = &svc.working_dir {
        t["working_dir"] = value(v.clone());
    }
    let mut env = toml_edit::InlineTable::new();
    for (k, v) in &svc.environment {
        env.insert(k, Value::from(v.clone()));
    }
    t["environment"] = value(env);
    t["auto_start"] = value(svc.auto_start);
    t["auto_restart"] = value(svc.auto_restart);
    let mut r = toml_edit::InlineTable::new();
    r.insert(
        "max_retries",
        Value::from(i64::from(svc.restart.max_retries)),
    );
    r.insert(
        "backoff_initial_ms",
        Value::from(svc.restart.backoff_initial_ms as i64),
    );
    r.insert(
        "backoff_max_ms",
        Value::from(svc.restart.backoff_max_ms as i64),
    );
    r.insert("backoff_factor", Value::from(svc.restart.backoff_factor));
    r.insert(
        "restart_window_secs",
        Value::from(svc.restart.restart_window_secs as i64),
    );
    t["restart"] = value(r);
    if let Some(crate::model::HealthCheck::Tcp {
        host,
        port,
        timeout_ms,
        interval_secs,
    }) = &svc.health
    {
        let mut h = toml_edit::InlineTable::new();
        h.insert("type", Value::from("tcp"));
        h.insert("host", Value::from(host.clone()));
        h.insert("port", Value::from(i64::from(*port)));
        h.insert("timeout_ms", Value::from(*timeout_ms as i64));
        h.insert("interval_secs", Value::from(*interval_secs as i64));
        t["health"] = value(h);
    }
    if let Some(v) = &svc.ui_url {
        t["ui_url"] = value(v.clone());
    }
    if let Some(v) = &svc.config_file {
        t["config_file"] = value(v.clone());
    }
    t["graceful_timeout_secs"] = value(svc.graceful_timeout_secs as i64);
    if let Some(v) = &svc.output_encoding {
        t["output_encoding"] = value(v.clone());
    }
    if let Some(v) = &svc.group {
        t["group"] = value(v.clone());
    }
    t["priority"] = value(i64::from(svc.priority));
    t
}

/// 原子写:临时文件 + rename。std::fs::rename 在 Windows 走
/// MoveFileExW(REPLACE_EXISTING),可直接覆盖已存在目标;目标被其他程序
/// 占用(编辑器/杀软句柄)时失败,错误带路径便于现场定位。
fn atomic_write(path: &Path, content: &str) -> WResult<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(WardenError::Config(format!(
            "写入 {} 失败(目标可能被其他程序占用):{e}",
            path.display()
        )));
    }
    Ok(())
}

/// 旧双源文件一次性迁移(文件为唯一数据源的重构收尾):
/// - `data_dir/runtime_services.toml`(旧运行时 overlay)→ 按 name 合并进配置文件;
/// - `data_dir/desired_state.json`(旧期望状态)→ `true` 的服务置 `auto_start = true`;
/// - 合并结果同时写回文件与内存 cfg(桌面版对 daemon 字段的覆盖不被文件重载冲掉);
/// - 成功后旧文件改名 `.bak`;写盘失败仅内存生效并强警告(CRUD 将不可用)。
///
/// 返回迁移后的配置文件路径(未发生迁移时返回原值)。
pub fn migrate_legacy_sources(
    cfg: &mut crate::config::Config,
    config_path: Option<PathBuf>,
    data_dir: &Path,
) -> Option<PathBuf> {
    if data_dir.as_os_str().is_empty() {
        return config_path;
    }
    let overlay_p = data_dir.join("runtime_services.toml");
    let desired_p = data_dir.join("desired_state.json");
    if !overlay_p.exists() && !desired_p.exists() {
        return config_path;
    }
    let target = config_path
        .clone()
        .unwrap_or_else(crate::config::default_config_create_path);
    let mut file = match ConfigFile::load_or_create(&target) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "[config] 旧数据迁移:打开 {} 失败({e}),跳过迁移",
                target.display()
            );
            return config_path;
        }
    };

    let mut merged = 0usize;
    if let Ok(text) = std::fs::read_to_string(&overlay_p) {
        // 与旧 read_runtime_services 相同的文件形状:{"service": [ ... ]}
        let list = toml::from_str::<HashMap<String, Vec<ServiceConfig>>>(&text)
            .ok()
            .and_then(|m| m.into_iter().next().map(|(_, v)| v))
            .unwrap_or_default();
        for svc in list {
            if let Some(e) = cfg.services.iter_mut().find(|s| s.name == svc.name) {
                *e = svc.clone();
            } else {
                cfg.services.push(svc.clone());
            }
            if let Err(e) = file.replace_or_append_service(&svc) {
                tracing::warn!("[config] 迁移服务 '{}' 写入失败:{e}", svc.name);
            } else {
                merged += 1;
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(&desired_p) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, bool>>(&text) {
            for (name, want) in map {
                if !want {
                    continue;
                }
                file.set_auto_start(&name, true);
                if let Some(s) = cfg.services.iter_mut().find(|s| s.name == name) {
                    s.auto_start = true;
                }
            }
        }
    }

    match file.save() {
        Ok(()) => {
            let _ = std::fs::rename(&overlay_p, data_dir.join("runtime_services.toml.bak"));
            let _ = std::fs::rename(&desired_p, data_dir.join("desired_state.json.bak"));
            tracing::info!(
                "[config] 已把 {merged} 个运行时服务与期望状态一次性迁移进 {} \
                 (旧文件改名 .bak;desired=true 已转为 auto_start=true)",
                target.display()
            );
            Some(target)
        }
        Err(e) => {
            tracing::warn!("[config] 旧数据迁移写盘失败({e}):本次会话仅内存生效,CRUD 写回不可用");
            config_path
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warden-cfgedit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn svc(name: &str, command: &str) -> ServiceConfig {
        ServiceConfig {
            name: name.into(),
            display_name: String::new(),
            description: String::new(),
            command: command.into(),
            args: vec![],
            working_dir: None,
            environment: HashMap::new(),
            auto_start: false,
            auto_restart: false,
            restart: Default::default(),
            health: None,
            ui_url: None,
            config_file: None,
            graceful_timeout_secs: 10,
            output_encoding: None,
            group: None,
            priority: 0,
        }
    }

    /// 意图:CRUD 写回唯一数据源的基本契约——新文件带说明头、可被 Config 解析、
    /// 字段(health/env/priority)round-trip 不丢。
    #[test]
    fn append_to_new_file_creates_parsable_config() {
        let dir = tmpdir("append");
        let path = dir.join("services.toml");
        let mut f = ConfigFile::load_or_create(&path).unwrap();
        let mut s = svc("a", "ping");
        s.priority = 7;
        s.group = Some("edge".into());
        s.environment.insert("K".into(), "V".into());
        s.health = Some(crate::model::HealthCheck::Tcp {
            host: "127.0.0.1".into(),
            port: 8080,
            timeout_ms: 500,
            interval_secs: 2,
        });
        f.append_service(&s).unwrap();
        f.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("warden 自动创建"), "新文件应带说明头:{text}");
        let cfg = crate::config::Config::parse(&text).unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "a");
        assert_eq!(cfg.services[0].priority, 7);
        assert_eq!(cfg.services[0].group.as_deref(), Some("edge"));
        assert_eq!(
            cfg.services[0].environment.get("K").map(String::as_str),
            Some("V")
        );
        assert!(cfg.services[0].health.is_some(), "health 应 round-trip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图:唯一数据源承诺的核心——编辑一个服务不得破坏其他条目的注释/排版。
    #[test]
    fn replace_preserves_other_entries_comments() {
        let dir = tmpdir("replace");
        let path = dir.join("services.toml");
        std::fs::write(
            &path,
            "# 顶部说明\n\n[daemon]\napi_bind = \"127.0.0.1:8789\"\n\n\
             # a 的注释:保留我\n[[service]]\nname = \"a\"\ncommand = \"ping\"\n\n\
             [[service]]\nname = \"b\"\ncommand = \"sleep\"\n",
        )
        .unwrap();

        let mut f = ConfigFile::load(&path).unwrap();
        f.replace_service(&svc("b", "nova")).unwrap();
        f.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# a 的注释:保留我"),
            "未触及条目的注释应保留:{text}"
        );
        assert!(text.contains("command = \"nova\""), "b 应已替换:{text}");
        let cfg = crate::config::Config::parse(&text).unwrap();
        assert_eq!(cfg.services.len(), 2);
        assert_eq!(cfg.services[1].command, "nova");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_and_missing_cases() {
        let dir = tmpdir("remove");
        let path = dir.join("services.toml");
        let mut f = ConfigFile::load_or_create(&path).unwrap();
        f.append_service(&svc("a", "x")).unwrap();
        f.append_service(&svc("b", "y")).unwrap();
        f.save().unwrap();

        let mut f = ConfigFile::load(&path).unwrap();
        f.remove_service("a").unwrap();
        f.save().unwrap();
        let cfg = crate::config::Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "b");

        // 删除不存在的条目报错
        let mut f = ConfigFile::load(&path).unwrap();
        assert!(f.remove_service("nope").is_err());
        // 替换不存在的条目报错
        assert!(f.replace_service(&svc("nope", "z")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 意图:迁移时 desired=true → auto_start=true 的写位正确(不存在条目则忽略)。
    #[test]
    fn set_auto_start_targets_named_entry_only() {
        let dir = tmpdir("autostart");
        let path = dir.join("services.toml");
        let mut f = ConfigFile::load_or_create(&path).unwrap();
        f.append_service(&svc("a", "x")).unwrap();
        f.append_service(&svc("b", "y")).unwrap();
        f.set_auto_start("b", true);
        f.set_auto_start("ghost", true); // 不存在:忽略
        f.save().unwrap();

        let cfg = crate::config::Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!cfg.services[0].auto_start, "a 不应被改动");
        assert!(cfg.services[1].auto_start, "b 应为 auto_start=true");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
