//! 验证 config/services.example.toml 可解析且含 rs-iot 三件套。

use std::path::PathBuf;

use warden::config::Config;

#[test]
fn example_config_parses_with_rsiot_trio() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/services.example.toml");
    let s = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {} 失败:{e}", path.display()));
    let cfg = Config::parse(&s).expect("example 应可解析");

    let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"rs-iot"), "应含 rs-iot,实际:{names:?}");
    assert!(names.contains(&"reasonix"), "应含 reasonix,实际:{names:?}");
    assert!(
        names.contains(&"rsiot-gateway"),
        "应含 rsiot-gateway,实际:{names:?}"
    );

    // 三件套默认 auto_start
    for svc in &cfg.services {
        assert!(svc.auto_start, "{} 应 auto_start", svc.name);
    }

    // daemon 配置可读
    assert!(!cfg.daemon.api_bind.is_empty());
}
