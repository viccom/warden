//! e2e:运行时 CRUD 写回配置文件(唯一数据源)+ 健康检查 + 服务配置文件端点。
//!
//! 1. CRUD:POST/PUT/DELETE /api/v1/services 全路径(含运行中拒删)——
//!    写回 services.toml(toml_edit 保注释),重建 state(daemon 重启模拟)后仍在;
//!    start/stop 不再产生任何持久化副作用(无 desired_state.json)。
//! 2. auto_start:唯一恢复机制(daemon 重启按 auto_start 拉起)。
//! 3. 迁移:旧 runtime_services.toml + desired_state.json 一次性并入配置文件。
//! 4. health:真 TcpListener 起本地端口,服务指向它 → healthy;关闭 → unhealthy(迁移)。

mod common;

use std::time::Duration;

use tower::ServiceExt; // oneshot
use warden::api::{build_router, build_state, AppState};
use warden::config::Config;

/// 建临时目录 + 真实配置文件(带注释,验证 CRUD 不破坏)+ build_state。
fn setup(tag: &str) -> (AppState, String, String) {
    let d = std::env::temp_dir().join(format!("warden-crudcfg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let data_dir = d.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let cfg_path = d.join("services.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "# 现场服务清单(唯一数据源)\n\
             [daemon]\ndata_dir = \"{}\"\nlog_dir = \"\"\n\n\
             # file-svc 的注释:CRUD 不得动我\n[[service]]\nname = \"file-svc\"\ncommand = \"whatever\"\nauto_start = false\n",
            data_dir.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let cfg = Config::load(Some(&cfg_path)).unwrap();
    let state = build_state(cfg, Some(cfg_path.clone()));
    (
        state,
        data_dir.to_string_lossy().replace('\\', "/"),
        cfg_path.to_string_lossy().replace('\\', "/"),
    )
}

async fn hit(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Response<axum::body::Body> {
    use axum::body::Body;
    use axum::http::Request;
    let b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    build_router(state.clone()).oneshot(req).await.unwrap()
}

fn read_cfg(path: &str) -> Config {
    Config::load(Some(std::path::Path::new(path))).unwrap()
}

/// CRUD 全路径写回配置文件;重建 state 后服务仍在;start/stop 零持久化副作用。
#[tokio::test]
async fn crud_writes_to_config_file_and_survives_restart() {
    let (state, data_dir, cfg_path) = setup("crud");
    let (cmd, args) = common::long_runner();

    // create:合法 body → 200,条目落盘
    let body = serde_json::json!({
        "name": "runtime-svc",
        "command": cmd,
        "args": args,
        "auto_start": false,
        "graceful_timeout_secs": 1,
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);
    let file_text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        file_text.contains("runtime-svc"),
        "CRUD 应写回配置文件:{file_text}"
    );
    // 唯一数据源:未触及条目的注释保留
    assert!(
        file_text.contains("# file-svc 的注释:CRUD 不得动我"),
        "未触及条目注释应保留:{file_text}"
    );
    // 不再产生 overlay 副作用文件
    assert!(
        !std::path::Path::new(&data_dir)
            .join("runtime_services.toml")
            .exists(),
        "不应再有 overlay 文件"
    );

    // create:重名拒绝(400/409 家族,非 200)且文件不重复写入
    let dup = serde_json::json!({ "name": "runtime-svc", "command": "x" });
    let r = hit(&state, "POST", "/api/v1/services", Some(dup)).await;
    assert_ne!(r.status(), 200, "重名应拒绝");
    assert_eq!(
        read_cfg(&cfg_path).services.len(),
        2,
        "拒绝后文件条目数不变"
    );

    // config 端点:返回完整配置(编辑表单预填用)
    let r = hit(&state, "GET", "/api/v1/services/runtime-svc/config", None).await;
    assert_eq!(r.status(), 200);
    {
        use axum::body::to_bytes;
        let bytes = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let cfg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cfg["name"], "runtime-svc");
        assert_eq!(cfg["command"], cmd, "config 应含完整 command");
    }

    // create:坏 name(禁用字符)拒绝
    let bad = serde_json::json!({ "name": "a/b", "command": "x" });
    let r = hit(&state, "POST", "/api/v1/services", Some(bad)).await;
    assert_ne!(r.status(), 200, "禁用字符 name 应拒绝");

    // update:名称不一致拒绝
    let up = serde_json::json!({ "name": "mismatch", "command": "y" });
    let r = hit(&state, "PUT", "/api/v1/services/runtime-svc", Some(up)).await;
    assert_ne!(r.status(), 200, "body.name 与路径不一致应拒绝");

    // update:一致 → 200,文件已替换
    let up = serde_json::json!({
        "name": "runtime-svc",
        "command": "updated-cmd",
        "args": [],
        "auto_restart": true,
        "restart": {
            "mode": "unexpected",
            "expected_exit_codes": [0, 130],
            "max_retries": 5,
            "backoff_initial_ms": 500,
            "backoff_max_ms": 30000,
            "backoff_factor": 3.0,
            "restart_window_secs": 120,
        },
    });
    let r = hit(&state, "PUT", "/api/v1/services/runtime-svc", Some(up)).await;
    assert_eq!(r.status(), 200);
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("updated-cmd"),
        "update 应写回文件"
    );

    // update:restart_policy 新字段(mode / expected_exit_codes 等)round-trip
    // 落盘 + 反序列化无损 —— 钓 PUT 写盘吞字段的回归。
    let cfg = read_cfg(&cfg_path);
    let svc = cfg
        .services
        .iter()
        .find(|s| s.name == "runtime-svc")
        .unwrap();
    use warden::model::RestartMode;
    assert_eq!(svc.restart.mode, RestartMode::Unexpected);
    assert_eq!(svc.restart.expected_exit_codes, vec![0, 130]);
    assert_eq!(svc.restart.max_retries, 5);
    assert_eq!(svc.restart.backoff_factor, 3.0);

    // delete:200,文件条目移除(真删,重启不复活)
    let r = hit(&state, "DELETE", "/api/v1/services/runtime-svc", None).await;
    assert_eq!(r.status(), 200);
    let names: Vec<String> = read_cfg(&cfg_path)
        .services
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert_eq!(names, vec!["file-svc"], "删除后文件只剩 file-svc:{names:?}");

    // 再造一个,重建 state(daemon 重启模拟)后仍在
    let body = serde_json::json!({ "name": "persist-svc", "command": cmd, "args": args });
    hit(&state, "POST", "/api/v1/services", Some(body)).await;
    let state2 = build_state(
        read_cfg(&cfg_path),
        Some(std::path::PathBuf::from(&cfg_path)),
    );
    let names = state2.supervisor.names();
    assert!(
        names.contains(&"persist-svc".to_string()),
        "重建后 CRUD 服务应恢复(来自配置文件):{names:?}"
    );
    assert!(
        names.contains(&"file-svc".to_string()),
        "file-svc 仍在:{names:?}"
    );

    // start/stop 零持久化副作用:不产生 desired_state.json
    let r = hit(&state, "POST", "/api/v1/services/persist-svc/start", None).await;
    assert_eq!(r.status(), 200);
    let _ = state.supervisor.stop_all().await;
    assert!(
        !std::path::Path::new(&data_dir)
            .join("desired_state.json")
            .exists(),
        "start 不应产生 desired_state.json"
    );

    let _ = std::fs::remove_dir_all(std::path::Path::new(&cfg_path).parent().unwrap());
}

/// 运行中的服务:PUT 允许保存配置(重启后生效,状态保留);DELETE 仍拒绝。
#[tokio::test]
async fn crud_allows_update_while_running_but_not_delete() {
    let (state, _data_dir, cfg_path) = setup("run");
    let (cmd, args) = common::long_runner();
    let body = serde_json::json!({
        "name": "busy",
        "command": cmd,
        "args": args,
        "graceful_timeout_secs": 1,
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);
    state.supervisor.start("busy").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if state
            .supervisor
            .list()
            .iter()
            .any(|s| s.name == "busy" && s.state.is_running())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "busy 未进入 running"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 运行中:PUT 允许保存配置(写回文件,重启后生效),进程状态保留
    let up = serde_json::json!({ "name": "busy", "command": "cmd.exe", "args": ["/c", "echo", "saved"] });
    let r = hit(&state, "PUT", "/api/v1/services/busy", Some(up)).await;
    assert_eq!(r.status(), 200, "运行中 PUT 应允许保存配置");
    assert!(
        state.supervisor.status("busy").unwrap().state.is_running(),
        "保存配置后服务应仍在运行"
    );
    let file_text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(file_text.contains("cmd.exe"), "文件应已更新:{file_text}");
    // 运行中:DELETE 仍 409
    let r = hit(&state, "DELETE", "/api/v1/services/busy", None).await;
    assert_eq!(r.status(), 409, "运行中 DELETE 仍应 409");
    state.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(std::path::Path::new(&cfg_path).parent().unwrap());
}

/// group/priority/ui_url/config_file 经 CRUD 写入 → 状态快照透出 → 文件重建后不丢。
#[tokio::test]
async fn crud_group_priority_roundtrip() {
    let (state, _data_dir, cfg_path) = setup("groupprio");
    let (cmd, args) = common::long_runner();
    let body = serde_json::json!({
        "name": "grouped-svc",
        "command": cmd,
        "args": args,
        "group": "edge",
        "priority": 7,
        "ui_url": "http://127.0.0.1:8790",
        "config_file": "C:/tmp/demo.toml",
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);

    let r = hit(&state, "GET", "/api/v1/services/grouped-svc", None).await;
    assert_eq!(r.status(), 200);
    {
        use axum::body::to_bytes;
        let bytes = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let s: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(s["group"], "edge", "状态快照应含 group:{s}");
        assert_eq!(s["priority"], 7, "状态快照应含 priority:{s}");
        assert_eq!(
            s["ui_url"], "http://127.0.0.1:8790",
            "状态快照应含 ui_url:{s}"
        );
        assert_eq!(
            s["config_file"], "C:/tmp/demo.toml",
            "状态快照应含 config_file:{s}"
        );
    }

    // 重建 state(daemon 重启模拟)后从文件恢复,group/priority 不丢
    let state2 = build_state(
        read_cfg(&cfg_path),
        Some(std::path::PathBuf::from(&cfg_path)),
    );
    let r = hit(&state2, "GET", "/api/v1/services/grouped-svc/config", None).await;
    assert_eq!(r.status(), 200);
    {
        use axum::body::to_bytes;
        let bytes = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let cfg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cfg["group"], "edge", "重建后 config 应保留 group:{cfg}");
        assert_eq!(cfg["priority"], 7, "重建后 config 应保留 priority:{cfg}");
    }

    let _ = std::fs::remove_dir_all(std::path::Path::new(&cfg_path).parent().unwrap());
}

/// auto_start 是唯一的重启恢复机制:daemon 重启(重建 state + start_auto)拉起它。
#[tokio::test]
async fn auto_start_is_the_only_recovery_source() {
    let (state, _data_dir, cfg_path) = setup("autostart");
    let (cmd, args) = common::long_runner();
    let body = serde_json::json!({
        "name": "boot-svc",
        "command": cmd,
        "args": args,
        "auto_start": true,
        "graceful_timeout_secs": 1,
    });
    let r = hit(&state, "POST", "/api/v1/services", Some(body)).await;
    assert_eq!(r.status(), 200);

    // 模拟 daemon 重启:重建 state + start_auto(serve 编排的启动步骤)
    let state2 = build_state(
        read_cfg(&cfg_path),
        Some(std::path::PathBuf::from(&cfg_path)),
    );
    state2.supervisor.start_auto().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if state2
            .supervisor
            .list()
            .iter()
            .any(|s| s.name == "boot-svc" && s.state.is_running())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "auto_start=true 的服务应在重启后拉起"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // file-svc(auto_start=false)不应被拉起
    assert_eq!(
        state2.supervisor.status("file-svc").unwrap().state.name(),
        "stopped"
    );

    state2.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(std::path::Path::new(&cfg_path).parent().unwrap());
}

/// 旧双源文件一次性迁移:overlay 并入配置文件;desired=true → auto_start=true;
/// 旧文件改名 .bak;内存与文件一致。
#[tokio::test]
async fn legacy_overlay_and_desired_migrate_into_config() {
    let (state_holder, data_dir, cfg_path) = setup("migrate");
    drop(state_holder); // setup 只为搭环境,迁移发生在下一次 build_state

    // 造旧文件:overlay 含 mig-svc(配置文件里没有)+ desired 标 file-svc=true
    std::fs::write(
        std::path::Path::new(&data_dir).join("runtime_services.toml"),
        r#"[[service]]
name = "mig-svc"
command = "ping"
args = ["-n", "60", "127.0.0.1"]
graceful_timeout_secs = 1
"#,
    )
    .unwrap();
    std::fs::write(
        std::path::Path::new(&data_dir).join("desired_state.json"),
        r#"{ "file-svc": true, "gone": true }"#,
    )
    .unwrap();

    let state = build_state(
        read_cfg(&cfg_path),
        Some(std::path::PathBuf::from(&cfg_path)),
    );

    // 迁移并入文件:mig-svc 存在、file-svc auto_start=true、注释保留
    let cfg = read_cfg(&cfg_path);
    assert!(
        cfg.services.iter().any(|s| s.name == "mig-svc"),
        "overlay 服务应并入文件:{:?}",
        cfg.services
    );
    let fs = cfg.services.iter().find(|s| s.name == "file-svc").unwrap();
    assert!(fs.auto_start, "desired=true 应转为 auto_start=true");
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("# file-svc 的注释:CRUD 不得动我"),
        "迁移不得破坏注释"
    );
    // 内存一致(supervisor 有 mig-svc,且 file-svc auto_start 已生效)
    assert!(state.supervisor.names().contains(&"mig-svc".to_string()));
    assert!(state.supervisor.status("file-svc").unwrap().auto_start);
    // 旧文件改名 .bak
    assert!(std::path::Path::new(&data_dir)
        .join("runtime_services.toml.bak")
        .exists());
    assert!(std::path::Path::new(&data_dir)
        .join("desired_state.json.bak")
        .exists());
    // "gone"(文件里没有的服务)被忽略,不产生新条目
    assert!(!state.supervisor.names().contains(&"gone".to_string()));

    let _ = std::fs::remove_dir_all(std::path::Path::new(&cfg_path).parent().unwrap());
}

/// 健康检查:真 TcpListener healthy → 关闭 unhealthy(状态迁移 + LogHub 告警行)。
#[tokio::test]
async fn health_check_transitions_and_alerts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hport = listener.local_addr().unwrap().port();
    let d = std::env::temp_dir().join(format!("warden-crudcfg-health-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let cfg_path = d.join("services.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[daemon]\ndata_dir = \"{d}\"\nlog_dir = \"\"\n\n\
             [[service]]\nname = \"h2\"\ncommand = \"{cmd}\"\nargs = [{args}]\ngraceful_timeout_secs = 2\n\
             health = {{ type = \"tcp\", host = \"127.0.0.1\", port = {hport}, timeout_ms = 500, interval_secs = 1 }}\n",
            d = d.to_string_lossy().replace('\\', "/"),
            cmd = if cfg!(windows) { "ping" } else { "sleep" },
            args = if cfg!(windows) {
                "\"-n\", \"30\", \"127.0.0.1\""
            } else {
                "\"30\""
            },
        ),
    )
    .unwrap();
    let state = build_state(
        Config::load(Some(&cfg_path)).unwrap(),
        Some(cfg_path.clone()),
    );
    let _h = warden::supervisor::health::spawn_health(state.supervisor.clone(), None);
    state.supervisor.start("h2").await.unwrap();
    let handle = state.supervisor.log_hub("h2").unwrap();

    // 等 health task 首查(1s tick + interval 1s)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        if state
            .supervisor
            .list()
            .iter()
            .any(|s| s.health.status == "healthy")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "未进入 healthy(端口开着)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // 关端口 → unhealthy + LogHub 告警行
    drop(listener);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        if state
            .supervisor
            .list()
            .iter()
            .any(|s| s.health.status == "unhealthy")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "未迁移 unhealthy(端口已关)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let alerts: Vec<String> = handle
        .snapshot(50)
        .iter()
        .filter(|l| l.text.contains("健康状态迁移"))
        .map(|l| l.text.clone())
        .collect();
    assert!(!alerts.is_empty(), "LogHub 应有健康迁移告警行");
    assert!(
        alerts.iter().any(|a| a.contains("unhealthy")),
        "含 unhealthy 迁移:{alerts:?}"
    );

    state.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(&d);
}

/// 配置文件端点:GET 读取(不存在返回 exists=false)/PUT 保存(toml 校验+格式化)/未配置 404。
#[tokio::test]
async fn config_file_get_put_roundtrip() {
    let (state, data_dir, cfg_path) = setup("cfgfile");

    // 服务未配 config_file → 404(语义:该服务不提供配置文件编辑)
    let r = hit(&state, "GET", "/api/v1/services/file-svc/config-file", None).await;
    assert_eq!(r.status(), 404, "未配置 config_file 应 404");

    // PUT 配上 config_file(CRUD update 写回文件)
    let f = format!("{data_dir}/demo.toml");
    let up = serde_json::json!({ "name": "file-svc", "command": "x", "config_file": f });
    let r = hit(&state, "PUT", "/api/v1/services/file-svc", Some(up)).await;
    assert_eq!(r.status(), 200);

    // 文件不存在:GET 返回 exists=false + 空内容(编辑器可创建)
    let r = hit(&state, "GET", "/api/v1/services/file-svc/config-file", None).await;
    assert_eq!(r.status(), 200);
    {
        use axum::body::to_bytes;
        let b: serde_json::Value =
            serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(b["exists"], false);
        assert_eq!(b["format"], "toml");
        assert_eq!(b["path"], f);
    }

    // PUT 保存(格式化):坏 toml → 400 且不落盘
    let bad = serde_json::json!({ "content": "not = = valid [[[", "format": true });
    let r = hit(
        &state,
        "PUT",
        "/api/v1/services/file-svc/config-file",
        Some(bad),
    )
    .await;
    assert_eq!(r.status(), 400, "坏 toml 应被校验拒绝");
    assert!(!std::path::Path::new(&f).exists(), "校验失败不应落盘");

    // PUT 保存(格式化):合法 toml → 落盘为规范格式
    let ok = serde_json::json!({ "content": "name='rs-iot'\nport=8790\n", "format": true });
    let r = hit(
        &state,
        "PUT",
        "/api/v1/services/file-svc/config-file",
        Some(ok),
    )
    .await;
    assert_eq!(r.status(), 200);
    let saved = std::fs::read_to_string(&f).unwrap();
    assert_eq!(
        saved, "name = \"rs-iot\"\nport = 8790\n",
        "应保存为规范 toml:{saved}"
    );

    // GET 回读存在的内容
    let r = hit(&state, "GET", "/api/v1/services/file-svc/config-file", None).await;
    {
        use axum::body::to_bytes;
        let b: serde_json::Value =
            serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(b["exists"], true);
        assert_eq!(b["content"], "name = \"rs-iot\"\nport = 8790\n");
    }

    // 非 UTF-8(GBK 等)文件:明确 400 而非 500,提示不支持在线编辑
    std::fs::write(&f, [0xD6, 0xD0, 0xCE, 0xC4]).unwrap(); // "中文" 的 GBK 字节
    let r = hit(&state, "GET", "/api/v1/services/file-svc/config-file", None).await;
    assert_eq!(r.status(), 400, "非 UTF-8 文件应 400(可读错误),非 500");
    {
        use axum::body::to_bytes;
        let b: serde_json::Value =
            serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert!(
            b["message"].as_str().unwrap_or("").contains("UTF-8"),
            "错误信息应说明编码问题:{b}"
        );
    }

    // 非 toml/json 格式(yaml):不做校验,原样保存
    let y = format!("{data_dir}/demo.yaml");
    let up = serde_json::json!({ "name": "file-svc", "command": "x", "config_file": y });
    hit(&state, "PUT", "/api/v1/services/file-svc", Some(up)).await;
    let body = serde_json::json!({ "content": "a: [unclosed\n", "format": true });
    let r = hit(
        &state,
        "PUT",
        "/api/v1/services/file-svc/config-file",
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 200, "yaml 不校验,原样保存");
    assert_eq!(std::fs::read_to_string(&y).unwrap(), "a: [unclosed\n");

    let _ = std::fs::remove_dir_all(std::path::Path::new(&cfg_path).parent().unwrap());
}
