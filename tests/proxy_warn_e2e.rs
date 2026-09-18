#![cfg(not(feature = "reverse-proxy"))]
//! P0 D8:feature off + [proxy] 配置存在 → 启动 warn 并忽略。
//! 断言方式:纯函数 `warden::should_warn_proxy_ignored`(见 lib.rs),
//! 不依赖 tracing 内部,测试更稳定。

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
    let _ = cfg; // 占位,Step 3 补完整断言
}
