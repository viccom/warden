#![cfg(not(feature = "reverse-proxy"))]
//! P0 D8:feature off + [proxy] 配置存在 → 启动 warn 并忽略。
//! 断言方式:纯函数 `warden::should_warn_proxy_ignored`(lib.rs,feature off 时可见),
//! 不依赖 tracing 内部,测试稳定。

use warden::config::Config;

#[test]
fn warns_when_proxy_config_present_but_feature_off() {
    let toml = r#"
[daemon]
api_bind = "127.0.0.1:0"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
"#;
    let cfg = Config::parse(toml).unwrap();
    assert!(cfg.proxy.is_some(), "配置层无条件解析,feature 不影响");
    assert!(
        warden::should_warn_proxy_ignored(&cfg),
        "feature off + [proxy] 存在 → 应判定 warn"
    );
}

#[test]
fn no_warn_without_proxy_section() {
    let cfg = Config::parse("[daemon]\napi_bind = \"127.0.0.1:0\"\n").unwrap();
    assert!(
        !warden::should_warn_proxy_ignored(&cfg),
        "无 [proxy] 段 → 不 warn"
    );
}
