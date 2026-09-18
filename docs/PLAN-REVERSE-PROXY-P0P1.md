# warden 反向代理 P0+P1 实施计划(TDD)

> **For Claude:** REQUIRED SUB-SKILL: 使用 `superpowers:executing-plans` 按任务逐条执行。
>
> **关联设计**:[`docs/PLAN-REVERSE-PROXY.md`](./PLAN-REVERSE-PROXY.md) —— 本计划的权威依据。决策 D1–D13 已在那里定案;本计划只覆盖 **P0(骨架) + P1(HTTP 反代 MVP,含 WebSocket/大文件流式)** 两阶段,P2(TLS)/P3(证书到期检测)后续另立计划。
>
> **Goal**:让 warden 在 `[proxy]` 段配置下,监听非标端口(http_bind),按请求 Host 路由(精确/通配/auto 从服务配置派生)反向代理到上游,流式透传 HTTP/SSE/WebSocket/大文件,未知 Host 421,服务停止 503。
>
> **Architecture**:`reverse-proxy` Cargo feature 门控;`[proxy]` 段与 `ServiceConfig.{proxy,subdomain}` 无条件解析(serde,零重依赖);引擎在 `src/proxy/` 下,挂进 `serve_with_shutdown` 的优雅停机链;转发用 hyper-util legacy client 流式直传,WebSocket 走 `hyper::upgrade::on` + `copy_bidirectional` 隧道。
>
> **Tech Stack**:hyper 1 / hyper-util 0.1 / http-body-util 0.1(feature 门控 optional 依赖);axum 0.8(既有);tokio(既有);tower(既有 dev-dep,需加 `util`)。
>
> **验证门槛**(CLAUDE.md):每步 `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test` 三项全绿,**双 feature 形态**(无 feature + `--features reverse-proxy`);编译一律 `--jobs 6`。
>
> **TDD 纪律**:每个任务严格 Red(写失败测试,跑一遍确认失败方式符合预期)→ Green(最小实现通过)→ Refactor(按需,再跑一遍)。不允许跳过 Red 验证失败方式这一步——这是 warden 既有 PLAN 文档的硬性要求(见 PLAN-GROUP-PRIORITY-PORTS.md)。
>
> **提交节奏**:每个 Task 末尾一次提交;提交信息前缀 `feat(proxy):` / `test(proxy):` / `chore(proxy):`。**未经用户明确要求不添加 Co-Authored-By 等署名尾注**(AGENTS.md)。

---

## 全局前置(开工前一次性确认)

- [ ] **P.0 工作目录与基线**:`cd /home/ncpe/dongit/warden`;`git status` 干净(无未提交改动);`cargo test --jobs 6` 基线全绿(当前 96 通过 + 1 ignored)。若不绿,先处理既有问题再开工,不要把既有问题混进本批次提交。
- [ ] **P.1 编译资源约束**:所有 `cargo build/test/clippy` 命令带 `--jobs 6`(AGENTS.md 硬性约束,16 核共享 KVM)。本计划下文所有命令默认已带,不再重复标注。
- [ ] **P.2 双 feature 验证约定**:每个 Task 末尾的验证步骤要跑**两套**——`cargo test --jobs 6`(无 feature,回归保障,D8)和 `cargo test --jobs 6 --features reverse-proxy`(本期新增)。两套都要绿。

---

## 阶段 P0:配置解析 + feature 骨架(无引擎,无新运行时依赖)

**本阶段目标**:配置层完整、双 feature 编译干净、引擎是空壳。**不引入任何 optional 依赖**(hyper 等)——它们在 P1 才加,P0 只动 config/model/Cargo.toml 的 `[features]` 段。

---

### Task 1:Cargo feature 段 + 空 proxy 模块骨架

**Files:**
- Modify: `Cargo.toml`(加 `[features]` 段)
- Create: `src/proxy/mod.rs`(空骨架)
- Modify: `src/lib.rs`(挂模块)

**Step 1: Red — 写一个编译期断言测试**

在 `src/lib.rs` 末尾(`init_tracing` 函数之后)加内联测试,验证 feature off 时 `proxy` 模块不存在:

```rust
#[cfg(not(feature = "reverse-proxy"))]
#[test]
fn proxy_module_absent_without_feature() {
    // 编译期断言:无 feature 时 mod proxy 不可见。若有人误把 cfg 去掉,此测试文件
    // 在无 feature 编译时仍会尝试引用 warden::proxy(下方 feature-on 测试),触发编译失败。
    // 这里用一个常量占位,真正断言在 Cargo feature 层面(下方 Task 2 的 config 单测
    // 不依赖 proxy mod,因此无 feature 时整个 crate 仍可编译)。
    assert!(true, "proxy module is feature-gated");
}
```

> 说明:feature 门控的"存在性"主要由 `cfg(feature)` 与编译矩阵保障,单测能做的有限。这一步更像是一个仪式化的锚点——真正的行为验证在 Task 2 的 config 单测(双 feature 共用)。

**Step 2: Run — 确认基线编译**

Run: `cargo check --jobs 6`
Expected: PASS(测试是 trivial 断言,但确认 lib.rs 改动没破坏编译)

**Step 3: Green — 加 feature 段与空模块**

`Cargo.toml` 在 `[dependencies]` 之前加:

```toml
[features]
default = ["reverse-proxy"]
reverse-proxy = []  # P0 空依赖;P1 起加 dep:hyper 等
```

`src/proxy/mod.rs`:

```rust
//! 反向代理模块(feature `reverse-proxy` 门控)。
//!
//! 设计与决策见 docs/PLAN-REVERSE-PROXY.md。P0 仅骨架;P1 起实现路由与转发。
```

`src/lib.rs` 在 `pub mod tui;` 之后加:

```rust
#[cfg(feature = "reverse-proxy")]
pub mod proxy;
```

**Step 4: Run — 双 feature 编译**

Run: `cargo check --jobs 6` && `cargo check --jobs 6 --features reverse-proxy`
Expected: 两条都 PASS

**Step 5: Run — clippy + fmt + test 双 feature**

Run:
```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --jobs 6
cargo test --jobs 6 --features reverse-proxy
```
Expected: 全绿(96 + 1 + 1 新增 trivial)

**Step 6: Commit**

```bash
git add Cargo.toml src/proxy/mod.rs src/lib.rs
git commit -m "feat(proxy): P0 骨架——reverse-proxy feature 段 + 空 mod"
```

---

### Task 2:配置 schema —— `[proxy]` 段解析与校验

**Files:**
- Modify: `src/config.rs`(加 `ProxyConfig`/`ProxyRoute` 结构 + 校验 + 单测)
- Test: `src/config.rs` 内联 `#[cfg(test)] mod tests`(既有)

**Step 1: Red — 写失败测试**

在 `src/config.rs` 的 `#[cfg(test)] mod tests` 内加(对齐既有 `config::tests` 风格):

```rust
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
    assert!(errs.iter().any(|e| e.contains("to 与 service") && e.contains("二选一")));
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
    assert!(errs.iter().any(|e| e.contains("ghost") && e.contains("不存在")));
}
```

> 注:测试里出现的 `validate_config` / `normalize_config` / `validate_config_with_services` 是本步要新增的公开函数(见 Step 3)。既有 config.rs 的 `validate_service` 是 per-service 的,这里要加一个针对 `ProxyConfig` 的同级校验函数。**先写测试,让它编译失败(函数不存在),再实现**——这就是 Red。

**Step 2: Run — 确认失败**

Run: `cargo test --jobs 6 config::tests::parse_proxy`
Expected: 编译失败(`ProxyConfig`/`ProxyRoute`/`validate_config` 等未定义)

**Step 3: Green — 实现结构 + 解析 + 校验**

在 `src/config.rs`:

1. `Config` 加字段:`#[serde(default)] pub proxy: Option<ProxyConfig>,`
2. 新增结构(对齐既有 `#[derive(Deserialize, Serialize, Clone, Debug)]` 风格):

```rust
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProxyConfig {
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub http_bind: Option<String>,
    #[serde(default)]
    pub https_bind: Option<String>,    // P2 起;P0/P1 解析但不消费
    #[serde(default = "default_proxy_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default)]
    pub preserve_host: bool,
    #[serde(default, rename = "route")]
    pub routes: Vec<ProxyRoute>,
}

fn default_proxy_connect_timeout_ms() -> u64 { 5000 }

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProxyRoute {
    pub host: String,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub preserve_host: Option<bool>,
}
```

3. 新增校验与规范化函数(签名见测试;实现遵循设计 §4.1 校验清单——host 小写化、禁 path、通配单层、to/service 二选一、重复 host、binds 均空 warn、service 引用不存在 warn)。**坏项 warn 不致命**,与 `validate_service` 的"跳过并 warn"语义一致。

**Step 4: Run — 测试通过**

Run: `cargo test --jobs 6 config::tests`
Expected: PASS(新增 9 个 + 既有 config 测试全绿)

**Step 5: Run — 双 feature 三项全绿**

Run: `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --jobs 6` + `cargo test --jobs 6 --features reverse-proxy`
Expected: 全绿

**Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat(proxy): P0 [proxy] 段解析与校验(to/service 二选一、host 规范化、通配单层)"
```

---

### Task 3:`ServiceConfig` 加 `proxy` / `subdomain` 字段(auto 路由来源)

**Files:**
- Modify: `src/model.rs`(`ServiceConfig` +2 字段)
- Modify: `src/config.rs`(校验:auto 暴露的服务 ui_url 缺失 warn、subdomain 非法 warn)
- Modify: `tests/api_flow.rs` 的 `service()` helper(补两字段,否则构造编译失败)
- Test: `src/model.rs` 内联 `#[cfg(test)]`(round-trip)

**Step 1: Red — round-trip 单测**

`src/model.rs` `#[cfg(test)] mod tests` 内加(对齐既有 `group`/`priority` round-trip 先例):

```rust
#[test]
fn proxy_subdomain_roundtrip() {
    let svc = ServiceConfig {
        name: "fs".into(),
        command: "x".into(),
        proxy: true,
        subdomain: Some("fs2".into()),
        ..Default::default()
    };
    let toml = toml::to_string(&svc).unwrap();
    let back: ServiceConfig = toml::from_str(&toml).unwrap();
    assert!(back.proxy, "proxy=true 保留");
    assert_eq!(back.subdomain.as_deref(), Some("fs2"), "subdomain 保留");
}

#[test]
fn proxy_fields_default() {
    let svc = ServiceConfig::default();
    assert!(!svc.proxy, "proxy 默认 false");
    assert!(svc.subdomain.is_none(), "subdomain 默认 None");
}
```

> 注:`ServiceConfig` 当前可能未 derive `Default`——检查既有代码;若无,本步一并补 `impl Default`(对齐 rs-iot 风格,`#[serde(default)]` 已在字段级,结构级 Default 是 round-trip 测试的 `..Default::default()` 依赖)。`ServiceConfig` 已有 `Default` 的话直接用。

**Step 2: Run — 确认失败**

Run: `cargo test --jobs 6 model::tests::proxy`
Expected: 编译失败(`proxy`/`subdomain` 字段不存在)

**Step 3: Green — 加字段**

`src/model.rs` `ServiceConfig` 加(对齐既有 `#[serde(default)]` 风格):

```rust
#[serde(default)]
pub proxy: bool,                // 允许经反代域名暴露(默认 false)
#[serde(default)]
pub subdomain: Option<String>,  // 下级域名标签;缺省取 name,约束 [a-z0-9]+
```

同步更新:
- `src/model.rs` `impl Default for ServiceConfig`(若存在)加 `proxy: false, subdomain: None`
- `tests/api_flow.rs` 的 `service()` helper 补 `proxy: false, subdomain: None`
- `src/config.rs` 校验:`proxy=true` 的服务 `ui_url` 缺失 → warn;`subdomain`(或缺省 name)小写化后不满足 `[a-z0-9]+` → warn(不暴露)

**Step 4: Run — 测试通过**

Run: `cargo test --jobs 6`
Expected: PASS(round-trip + 既有全绿;api_flow helper 更新后编译过)

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add src/model.rs src/config.rs tests/api_flow.rs
git commit -m "feat(proxy): P0 ServiceConfig 加 proxy/subdomain 字段(auto 路由来源)"
```

---

### Task 4:feature off + `[proxy]` 配置存在 → 启动 warn

**Files:**
- Modify: `src/lib.rs`(`serve_with_shutdown` 内加检测分支)
- Test: `tests/proxy_warn_e2e.rs`(新文件,`#![cfg(not(feature = "reverse-proxy"))]`)

**Step 1: Red — e2e 断言 warn 输出**

`tests/proxy_warn_e2e.rs`:

```rust
#![cfg(not(feature = "reverse-proxy"))]
//! P0 D8:feature off + [proxy] 配置存在 → 启动 warn 并忽略。
//! 用 logs capture 或 tracing 试验层断言"忽略 [proxy]"字样。

use warden::config::Config;

#[tokio::test]
async fn warns_when_proxy_config_present_but_feature_off() {
    // 构造含 [proxy] 的配置,走 build_state / serve 检测分支。
    // 断言:tracing 层捕获到含"reverse-proxy feature 未编译"的 warn 行。
    // (具体捕获手法见 Step 3——用 tracing_subscriber 的 mock layer 或
    //  warden 既有的 LogHub 间接观测;本步先写测试意图,实现时落具体断言。)
    let toml = r#"
[daemon]
api_bind = "127.0.0.1:0"
[proxy]
domain = "x.example.com"
http_bind = "0.0.0.0:8080"
"#;
    let cfg = Config::parse(toml).unwrap();
    assert!(cfg.proxy.is_some(), "配置层无条件解析,feature 不影响");
    // TODO: 启动 serve_with_shutdown(短超时),捕获 warn 行。
    //       实现时确定捕获方式(tracing test layer vs LogHub)。
    let _ = cfg; // 占位,Step 3 补完整断言
}
```

> 说明:这一步的 Red 是"测试存在但断言不完整"——它在无 feature 编译下存在,提醒实现者补 warn 分支。**Step 3 必须把 TODO 落成真实断言**,否则测试是空壳(违反"测试要验证意图")。

**Step 2: Run — 确认编译**

Run: `cargo test --jobs 6 --test proxy_warn_e2e`
Expected: 编译通过,测试跑过(空断言)——这是 Red 的弱形态,Step 3 会强化

**Step 3: Green — 加 warn 分支**

`src/lib.rs` `serve_with_shutdown` 开头(在 `build_state` 之后,`start_auto` 之前)加:

```rust
#[cfg(not(feature = "reverse-proxy"))]
if state.config_proxy_present() {  // 新增的只读访问(见下)
    tracing::warn!(
        "[warden] reverse-proxy feature 未编译,配置中的 [proxy] 段将被忽略"
    );
}
```

> 实现注:`AppState` 不直接持有 `Config`(它被 `build_state` 消费进 Supervisor)。需要一条路径让 `serve_with_shutdown` 知道 `[proxy]` 是否存在——选项:(a) `AppState` 加 `proxy_present: bool` 字段(build_state 时填);(b) 在 `cfg` 被 consume 前于 `serve_with_shutdown` 内检测。选 (a),与既有 `title`/`data_dir` 字段同风格(`AppState` 已有这类"从 cfg 抽出的只读快照")。

测试侧补完整断言:用 `tracing_subscriber::EnvFilter` + 一个共享 `Vec<String>` 收集 warn 行(参考 warden 既有 `logs.rs` 测试或 tracing mock 范式);若 warden 既有测试无此范式,改用更简单的方案——把 warn 检测抽成纯函数 `pub fn should_warn_proxy_ignored(cfg: &Config) -> bool`(feature off 时可见),e2e 直接断言该函数返回 true。**推荐抽纯函数**,测试更稳定、不依赖 tracing 内部。

**Step 4: Run — 测试通过**

Run: `cargo test --jobs 6 --test proxy_warn_e2e` + `cargo test --jobs 6 --features reverse-proxy --test proxy_warn_e2e`(后者整个文件被 `#![cfg(not)]` 跳过,0 测试)
Expected: 无 feature 形态 PASS;有 feature 形态 0 测试通过

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add src/lib.rs src/api/mod.rs tests/proxy_warn_e2e.rs
git commit -m "feat(proxy): P0 feature off + [proxy] 配置存在 → 启动 warn 忽略"
```

---

### Task 5:P0 收尾 —— example 配置 + ROADMAP 同步

**Files:**
- Modify: `config/services.example.toml`(加 `[proxy]` 注释段)
- Modify: `docs/ROADMAP.md`(加 Phase 6 反向代理条目 + 变更日志)

**Step 1: 加 example 注释**

在 `config/services.example.toml` 的 `[daemon]` 段之后加注释段(对齐既有注释风格——字段级 `#` 注释):

```toml
# ── 反向代理(reverse-proxy feature,默认编译进 release)─────────────────
# 设计与决策见 docs/PLAN-REVERSE-PROXY.md。本段缺省 = 不启用反代。
# [proxy]
# domain    = "opc.dongx.site"              # 根级域名:auto 路由 = <subdomain>.<domain>
# http_bind  = "0.0.0.0:8080"               # 生产目标 80;本机 80 被 OpenResty 占用 → 8080
# # https_bind = "0.0.0.0:8443"            # P2 起;443 同被占用,实测 8443 空闲
# # connect_timeout_ms = 5000              # 上游连接超时(缺省 5000)
# # preserve_host = false                 # 转发时 Host 重写为上游;true = 保留客户端 Host
# [[proxy.route]]                          # 显式路由(可多条;与 auto 并存时显式优先)
# host = "fs2.opc.dongx.site"
# to   = "http://127.0.0.1:9000"           # 显式上游;或 service = "rs-iot" 引用被监护服务
```

在某个 `[[service]]` 示例里加 auto 路由字段注释:

```toml
# proxy = true                              # 允许经反代域名暴露(默认 false)
# subdomain = "fs2"                         # 下级域名标签,约束 [a-z0-9]+;缺省 = name
```

**Step 2: 加 ROADMAP Phase 6 条目**

`docs/ROADMAP.md` 在 Phase 5 之后加:

```markdown
## Phase 6 —— 反向代理(基于域名的 L7 反代)

> 设计见 [PLAN-REVERSE-PROXY.md](./PLAN-REVERSE-PROXY.md);P0+P1 实施计划见 [PLAN-REVERSE-PROXY-P0P1.md](./PLAN-REVERSE-PROXY-P0P1.md)。

- [x] **P0 骨架 ✅(2026-09-18)**:reverse-proxy feature 段 + `[proxy]` 段解析/校验 + `ServiceConfig.{proxy,subdomain}` + feature off warn。配置层无条件解析(双 feature 一致),引擎空壳待 P1。
- [ ] P1 HTTP 反代 MVP(host 路由 + hyper-util 流式直传 + WebSocket + 421/503)
- [ ] P2 TLS 终止(单张通配证书 + mtime 热重载)
- [ ] P3 证书到期检测(1Panel 外部托管协同)
```

并在「最后更新」「当前阶段」行更新日期与进度。变更日志区加一条:

```markdown
- **2026-09-18(Phase 6 P0 ✅)**:反向代理功能启动。P0 骨架——reverse-proxy Cargo feature(默认开,desktop 退 default-features=false);`[proxy]` 段(domain/http_bind/https_bind/connect_timeout_ms/preserve_host/routes)与 `ServiceConfig.{proxy,subdomain}` 无条件解析,校验与既有 validate 同风格(坏项 warn 不致命);feature off + 配置存在 → 启动 warn 忽略。引擎空壳,P1 起实现。决策 D1–D13 见 PLAN-REVERSE-PROXY.md。
```

**Step 3: Run — example 解析测试回归**

Run: `cargo test --jobs 6 --test read_example`
Expected: PASS(example 加了注释段不应影响解析,read_example 只断言 services)

**Step 4: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 5: Commit**

```bash
git add config/services.example.toml docs/ROADMAP.md
git commit -m "docs(proxy): P0 example 配置注释 + ROADMAP Phase 6 条目"
```

---

## 阶段 P1:HTTP 反代 MVP(路由 + 流式转发 + WebSocket)

**本阶段目标**:`[proxy]` 配置下,warden 监听 `http_bind`,按 Host 路由到上游,流式透传 HTTP/SSE/WS/大文件,未知 Host 421,引用服务的路由在服务停止时返回 503。**引入 optional 依赖**(hyper/hyper-util/http-body-util)。

**P1 内部子阶段顺序**(重要):Task 6 加依赖与模块 → Task 7 路由核心(纯函数,单测)→ Task 8 转发层(头处理,单测)→ Task 9 监听编排 + 421/502/503 错误页 → Task 10 e2e HTTP/SSE/大文件 → Task 11 WebSocket → Task 12 收尾(CI/desktop/example)。

---

### Task 6:引入 optional 依赖 + proxy 模块子文件骨架

**Files:**
- Modify: `Cargo.toml`(`reverse-proxy` feature 加 dep)
- Create: `src/proxy/{router,forward}.rs`
- Modify: `src/proxy/mod.rs`(挂子模块)

**Step 1: Red — 编译期断言依赖存在**

`src/proxy/mod.rs`:

```rust
//! 反向代理模块。设计见 docs/PLAN-REVERSE-PROXY.md。
pub mod router;
pub mod forward;

// 编译期断言:hyper/hyper-util 在本 feature 下作为依赖可见。
const _: fn() = || {
    fn _assert_hyper() -> hyper::Request<hyper::body::Incoming> {
        unreachable!()
    }
    fn _assert_hyper_util() {
        let _ = hyper_util::client::legacy::Client::builder;
    }
};
```

**Step 2: Run — 确认失败**

Run: `cargo check --jobs 6 --features reverse-proxy`
Expected: 编译失败(hyper/hyper-util 未声明依赖)

**Step 3: Green — 加依赖与空子模块**

`Cargo.toml`:

```toml
[features]
default = ["reverse-proxy"]
reverse-proxy = ["dep:hyper", "dep:hyper-util", "dep:http-body-util"]
```

`[dependencies]` 加:

```toml
hyper = { version = "1", optional = true, features = ["client", "http1", "server"] }
hyper-util = { version = "0.1", optional = true, features = ["client", "client-legacy", "http1", "tokio"] }
http-body-util = { version = "0.1", optional = true }
```

`src/proxy/router.rs` 与 `src/proxy/forward.rs` 各加一行 `//! doc` 占位(让 mod 编译过)。

`src/lib.rs` 的 `pub mod proxy;` 保持(已在 Task 1 加)。

**Step 4: Run — 双 feature 编译**

Run: `cargo check --jobs 6` + `cargo check --jobs 6 --features reverse-proxy`
Expected: 两条都 PASS(无 feature 时 proxy mod 被 cfg 掉,不引用 hyper)

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add Cargo.toml src/proxy/
git commit -m "feat(proxy): P1 引入 hyper/hyper-util/http-body-util optional 依赖 + 子模块骨架"
```

---

### Task 7:路由匹配核心 `HostRouter::resolve`(纯函数,单测重灾区)

**Files:**
- Modify: `src/proxy/router.rs`
- Test: `src/proxy/router.rs` 内联 `#[cfg(test)] mod tests`

**Step 1: Red — 写失败测试**

`src/proxy/router.rs`:

```rust
//! Host 路由匹配核心(纯函数,无 IO,单测覆盖所有分支)。
//!
//! 设计见 PLAN-REVERSE-PROXY.md §4.3。匹配顺序:
//! 1. 精确 host 命中 → 该路由
//! 2. 通配 host(单层)命中 → 最长后缀胜出
//! 3. auto:<sub>.<domain> 且 sub 命中某 proxy=true 服务的 subdomain → AutoService
//! 4. 全不中 → NotFound(调用方返回 421)

use crate::config::ProxyConfig;
use crate::supervisor::Supervisor;
use std::sync::Arc;

/// 路由解析结果。
pub enum Decision {
    Route { to: String, preserve_host: bool },
    AutoService { name: String },
    NotFound,
}

pub struct HostRouter {
    cfg: ProxyConfig,
    supervisor: Arc<Supervisor>,
}

impl HostRouter {
    pub fn new(cfg: ProxyConfig, supervisor: Arc<Supervisor>) -> Self {
        Self { cfg, supervisor }
    }

    /// 规范化 host:转小写、剥离 :port。
    pub fn normalize_host(host: &str) -> String {
        let host = host.to_lowercase();
        match host.rfind(':') {
            Some(i) => host[..i].to_string(),
            None => host,
        }
    }

    /// 解析路由决策。host 须已 normalize。
    pub fn resolve(&self, host: &str) -> Decision {
        todo!("Step 3 实现")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(domain: &str, routes: &[(&str, &str)]) -> HostRouter {
        let cfg = ProxyConfig {
            domain: Some(domain.into()),
            http_bind: None,
            https_bind: None,
            connect_timeout_ms: 5000,
            preserve_host: false,
            routes: routes.iter().map(|(h, t)| crate::config::ProxyRoute {
                host: h.into(),
                to: Some(t.into()),
                service: None,
                preserve_host: None,
            }).collect(),
        };
        // 注意:HostRouter 持有 Supervisor 仅用于 auto 分支;本批单测不测 auto,
        // 传一个空 Supervisor 即可。若 Supervisor::new 签名不便,改用 mock trait。
        // (实现时确定:若 HostRouter 对 supervisor 的依赖只在 auto 分支,
        //  可把 resolve 拆成 resolve_route(&str)->Decision(纯函数,不持 supervisor)
        //  与 resolve_auto 单独两段,单测覆盖前者。)
        HostRouter::new(cfg, Arc::new(Supervisor::new(std::path::PathBuf::from(""))))
    }

    #[test]
    fn normalize_strips_port_and_lowercases() {
        assert_eq!(HostRouter::normalize_host("A.X.com:8443"), "a.x.com");
        assert_eq!(HostRouter::normalize_host("Plain.Host"), "plain.host");
    }

    #[test]
    fn exact_host_wins() {
        let r = router("x.com", &[("a.x.com", "http://1")]);
        assert!(matches!(r.resolve("a.x.com"), Decision::Route { .. }));
    }

    #[test]
    fn wildcard_single_label_matches() {
        let r = router("x.com", &[("*.x.com", "http://1")]);
        assert!(matches!(r.resolve("a.x.com"), Decision::Route { .. }));
    }

    #[test]
    fn wildcard_rejects_multilevel() {
        let r = router("x.com", &[("*.x.com", "http://1")]);
        assert!(matches!(r.resolve("a.b.x.com"), Decision::NotFound),
            "通配只匹配单层,a.b.x.com 不应命中 *.x.com");
    }

    #[test]
    fn exact_overrides_wildcard() {
        let r = router("x.com", &[
            ("*.x.com", "http://wild"),
            ("a.x.com", "http://exact"),
        ]);
        match r.resolve("a.x.com") {
            Decision::Route { to, .. } => assert_eq!(to, "http://exact"),
            _ => panic!("精确应胜出"),
        }
    }

    #[test]
    fn longest_wildcard_suffix_wins() {
        let r = router("x.com", &[
            ("*.x.com", "http://short"),
            ("*.sub.x.com", "http://long"),
        ]);
        match r.resolve("a.sub.x.com") {
            Decision::Route { to, .. } => assert_eq!(to, "http://long"),
            _ => panic!("最长后缀应胜出"),
        }
    }

    #[test]
    fn unknown_host_returns_not_found() {
        let r = router("x.com", &[("a.x.com", "http://1")]);
        assert!(matches!(r.resolve("b.x.com"), Decision::NotFound));
    }

    #[test]
    fn auto_branch_when_subdomain_matches_service() {
        // 本测依赖 Supervisor 状态:注册一个 proxy=true、subdomain="fs" 的服务。
        // 若 Step 3 决定把 auto 拆成纯函数(传入 subdomain 集合),本测改用纯函数
        // 断言。两种实现都行,选其一并在 Step 3 确定。
        // TODO Step 3:补完整实现。
    }

    #[test]
    fn auto_ignores_service_not_exposed() {
        // proxy=false 的服务不进 auto 路由。
        // TODO Step 3。
    }
}
```

> **实现注(Step 3 必读)**:测试里留了两个 TODO(auto 分支)。Step 3 必须落完整断言,不能留空。推荐实现:**把 `resolve` 拆成两层**——`resolve_route(host) -> Option<RouteMatch>`(纯函数,不持 supervisor,覆盖精确/通配/NotFound)与 `resolve_auto(host) -> Option<String>`(查 supervisor 的 proxy=true 服务列表,匹配 subdomain)。单测覆盖 `resolve_route`;auto 的 e2e 在 Task 10 覆盖(因为它依赖真实 Supervisor + 服务状态)。本 Task 的 auto 单测可简化为对 `resolve_auto` 输入 host、输出服务名的纯逻辑断言(若 Supervisor 提供一个"列出 proxy=true 服务的 (subdomain, name) 快照"的方法)。

**Step 2: Run — 确认失败**

Run: `cargo test --jobs 6 --features reverse-proxy proxy::router::tests`
Expected: 编译失败或 `resolve` 调 `todo!()` panic

**Step 3: Green — 实现 `normalize_host` + `resolve`**

按设计 §4.3 顺序实现。关键点:
- 精确匹配:线性扫 routes,host == route.host
- 通配匹配:`route.host` 以 `*.` 开头时,取后缀,host 末尾匹配后缀且剩余部分**不含 `.`**(单层)
- 最长后缀:通配命中多条时取 `route.host.len()` 最大者
- `Decision::Route.to`:从 `ProxyRoute.to` 取(显式 `to`)或从 `service` 引用解析(本 Task 暂不解析 service,留 TODO 给 Task 10 的 e2e——单测只覆盖 `to` 形态)

**Step 4: Run — 测试通过**

Run: `cargo test --jobs 6 --features reverse-proxy proxy::router::tests`
Expected: PASS

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add src/proxy/router.rs
git commit -m "feat(proxy): P1 HostRouter 路由匹配核心(精确/通配/auto,纯函数单测)"
```

---

### Task 8:转发层头处理 `forward::rewrite_headers`(纯函数,单测)

**Files:**
- Modify: `src/proxy/forward.rs`
- Test: `src/proxy/forward.rs` 内联 `#[cfg(test)] mod tests`

**Step 1: Red — 写失败测试**

`src/proxy/forward.rs`:

```rust
//! 转发层:头处理 + 流式直传 + WebSocket 隧道。
//!
//! 设计见 PLAN-REVERSE-PROXY.md §4.4。本 Task 只做头处理(纯函数,单测);
//! 流式直传与 WebSocket 在 Task 9/11 集成。

use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// hop-by-hop 头(RFC 7230 §6.1 + 常见代理专用头)。
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",  // 注意:WebSocket 升级路径单独处理,非升级请求剥掉
];

/// 重写请求头:剥离 hop-by-hop + Connection 令牌、重写 Host、追加 X-Forwarded-*。
///
/// - `client_ip`:客户端 IP(从 axum ConnectInfo 取)
/// - `upstream_authority`:上游 host:port(从 to URL 解析)
/// - `preserve_host`:true 时保留原 Host,false 时设为 upstream_authority
pub fn rewrite_headers(
    headers: &mut HeaderMap,
    client_ip: &str,
    upstream_authority: &str,
    scheme: &str,            // "http" | "https"(TLS 终止后)
    preserve_host: bool,
) {
    todo!("Step 3 实现")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("connection", "keep-alive, X-Custom".parse().unwrap());
        h.insert("x-custom", "v".parse().unwrap());
        h.insert("host", "a.example.com".parse().unwrap());
        h.insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
        h
    }

    #[test]
    fn strips_hop_by_hop() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert!(h.get("connection").is_none(), "Connection 被剥");
        assert!(h.get("keep-alive").is_none());
    }

    #[test]
    fn strips_connection_tokens() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert!(h.get("x-custom").is_none(), "Connection 令牌列出的头也要剥");
    }

    #[test]
    fn rewrites_host_by_default() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert_eq!(h.get("host").unwrap(), "upstream:8080");
    }

    #[test]
    fn preserves_host_when_configured() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", true);
        assert_eq!(h.get("host").unwrap(), "a.example.com");
    }

    #[test]
    fn appends_xff_with_comma() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        let xff = h.get("x-forwarded-for").unwrap().to_str().unwrap();
        assert_eq!(xff, "10.0.0.1, 203.0.113.5", "已有 XFF 以逗号续接");
    }

    #[test]
    fn adds_xff_when_absent() {
        let mut h = HeaderMap::new();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "http", false);
        assert_eq!(h.get("x-forwarded-for").unwrap(), "203.0.113.5");
    }

    #[test]
    fn sets_xfp_and_xfh() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert_eq!(h.get("x-forwarded-proto").unwrap(), "https");
        assert_eq!(h.get("x-forwarded-host").unwrap(), "a.example.com");
    }
}
```

**Step 2: Run — 确认失败**

Run: `cargo test --jobs 6 --features reverse-proxy proxy::forward::tests`
Expected: panic(todo!())

**Step 3: Green — 实现头处理**

关键点:
- 先处理 `Connection` 头:取其值按逗号 split,每个 token 是要剥的头名,连同 `Connection` 自身一起从 headers 移除
- 再剥 `HOP_BY_HOP` 里其余头
- `Host`:preserve_host=true 不动;否则设为 `upstream_authority`
- `X-Forwarded-For`:已有则 `, <client_ip>` 续接;无则插入
- `X-Forwarded-Proto` = scheme,`X-Forwarded-Host` = 原始 Host(在改写前捕获)

**Step 4: Run — 测试通过**

Run: `cargo test --jobs 6 --features reverse-proxy proxy::forward::tests`
Expected: PASS(7 个)

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add src/proxy/forward.rs
git commit -m "feat(proxy): P1 转发头处理(hop-by-hop 剥离 + X-Forwarded-* 追加,纯函数单测)"
```

---

### Task 9:转发 handler + 错误页(421/502/503)+ 监听编排

**Files:**
- Modify: `src/proxy/forward.rs`(加 `proxy_handler` + 错误页生成)
- Modify: `src/proxy/mod.rs`(加 `spawn` 编排:绑定 listener、建 Router、挂 shutdown)
- Modify: `src/lib.rs`(`serve_with_shutdown` 调 `proxy::spawn`)
- Test: `tests/proxy_e2e.rs`(新文件,`#![cfg(feature = "reverse-proxy")]`)

> 本 Task 是 P1 最大块。建议**先写一个最简 e2e**(proxy 一条显式路由到本地上游,断言 body 透传 + XFF 头),让它 Red,然后实现 handler 与编排让它 Green;后续 Task 10 再补 SSE/大文件/421/503 等用例。**不要一次写全 9 个 e2e 再实现**——那样 Red 阶段太长,违反 TDD 小步节奏。

**Step 1: Red — 最简 e2e**

`tests/proxy_e2e.rs`:

```rust
#![cfg(feature = "reverse-proxy")]
//! 反向代理 e2e。本地上游用 tokio::spawn + axum 随机端口。

mod common;

use axum::{routing::get, Router};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use warden::config::{Config, ProxyConfig, ProxyRoute};
use warden::supervisor::Supervisor;

/// 起一个本地上游,回显收到的头 + 固定 body。
async fn echo_upstream() -> (axum::Router, SocketAddr) {
    let app = Router::new().route("/", get(||async move {
        format!("upstream-ok")
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (app, addr)
}

#[tokio::test]
async fn proxies_get_body_and_adds_forwarded_headers() {
    let (_up, up_addr) = echo_upstream().await;
    // 构造 [proxy] 一条显式路由 host=fs.opc.dongx.site → http://up_addr
    let cfg = Config {
        daemon: Default::default(),
        services: vec![],
        proxy: Some(ProxyConfig {
            domain: Some("opc.dongx.site".into()),
            http_bind: Some("127.0.0.1:0".into()),  // 随机端口,勿固定
            https_bind: None,
            connect_timeout_ms: 2000,
            preserve_host: false,
            routes: vec![ProxyRoute {
                host: "fs.opc.dongx.site".into(),
                to: Some(format!("http://{up_addr}")),
                service: None,
                preserve_host: None,
            }],
        }),
    };
    // 走 warden 的 proxy::spawn(或等价编排),拿一个代理 listener
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let supervisor = Arc::new(Supervisor::new(std::path::PathBuf::from("")));
    let shutdown = tokio_util::sync::CancellationToken::new();
    warden::proxy::spawn(cfg.proxy.unwrap(), supervisor, proxy_listener, shutdown.clone());

    // 客户端请求
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .header("host", "fs.opc.dongx.site")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "upstream-ok");
    // XFF 断言:上游若回显头可查;本例上游不回显,留到 Task 10 补回显上游
}
```

> 注:`warden::proxy::spawn` 的签名在 Step 3 确定;测试先按设想写,编译失败即 Red。reqwest 已是 warden 依赖(TUI 用),e2e 可直接用。

**Step 2: Run — 确认失败**

Run: `cargo test --jobs 6 --features reverse-proxy --test proxy_e2e`
Expected: 编译失败(`warden::proxy::spawn` 不存在)

**Step 3: Green — 实现 handler + spawn 编排**

`src/proxy/forward.rs` 加 `proxy_handler`(axum handler):取 Host 头 → `HostRouter::resolve` → 按 Decision 分支:
- `Route { to, preserve_host }`:解析 to URL,`rewrite_headers`,`hyper_util::client::legacy::Client` 发请求,流式透传响应 body(`Response::from_parts` + `Body` 桥接)
- `AutoService { name }`:查 Supervisor snapshot,服务 Running 且有 ui_url → 用 ui_url 作 to 走 Route 分支;非 Running → 503 错误页;ui_url 缺失 → 502 错误页;服务不存在 → 404
- `NotFound`:421 错误页

错误页:极简 HTML/文本,含 host、上游(若有)、原因、服务状态(若 503)。

`src/proxy/mod.rs` 加 `pub async fn spawn(cfg, supervisor, listener, shutdown)`:
- 建 `HostRouter`(Arc 共享)
- 建 hyper-util Client(builder,连接池,connect_timeout)
- axum Router with fallback = proxy_handler(代理不区分 path,所有请求进 handler)
- `axum::serve(listener, app).with_graceful_shutdown(...)` + drain 上限 15s(对齐设计 §4.5)

`src/lib.rs` `serve_with_shutdown`:在 `state.supervisor.start_auto()` 之后、axum API serve 之前,若 `cfg.proxy` 存在且 feature 开,绑 `http_bind` listener 调 `proxy::spawn`(tokio::spawn 后台跑);shutdown 与既有 stop_task 并行。

**Step 4: Run — 最简 e2e 通过**

Run: `cargo test --jobs 6 --features reverse-proxy --test proxy_e2e`
Expected: PASS

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add src/proxy/forward.rs src/proxy/mod.rs src/lib.rs tests/proxy_e2e.rs
git commit -m "feat(proxy): P1 转发 handler + 监听编排 + 错误页(421/502/503)"
```

---

### Task 10:e2e 补全 —— SSE 流式 / 大文件 / 421 / 503 / auto / 通配

**Files:**
- Modify: `tests/proxy_e2e.rs`(补用例)

**本 Task 是 e2e 的主体。每个用例独立一个 `#[tokio::test]`,Red-Green 节奏:写用例 → 跑(若失败是 Red)→ 修实现 → Green。多数用例应在 Task 9 的实现下直接 Green(设计已覆盖),失败则暴露实现缺陷。**

**Step 1: Red — 逐个加用例**

加以下用例(参考设计 §5 Task 6 列表):

```rust
#[tokio::test]
async fn streams_sse_without_buffering() {
    // 上游:axum SSE 流,每 200ms 发一条事件,共 3 条。
    // 断言:客户端收到首条事件的时刻 < 300ms(若被缓冲会接近 600ms)。
    // 用 Instant 测时序,不用 sleep 对齐。
}

#[tokio::test]
async fn streams_large_request_body() {
    // 上游:回显 body 的 sha256。
    // 客户端:上传 8 MiB 随机字节。
    // 断言:上游回显的哈希 == 客户端发送的哈希(零缓冲/零截断)。
}

#[tokio::test]
async fn returns_421_for_unknown_host() {
    // [proxy] 只配了 fs.opc.dongx.site,请求 b.opc.dongx.site → 421。
}

#[tokio::test]
async fn returns_502_when_upstream_refused() {
    // to 指向一个无人监听的端口 → 502。
}

#[tokio::test]
async fn auto_service_stopped_returns_503_with_state() {
    // 注册 proxy=true、ui_url=本地上游的服务,不 start。
    // 请求 <sub>.<domain> → 503,页面含状态文案("Stopped" 等)。
}

#[tokio::test]
async fn auto_uses_subdomain_override() {
    // 服务 name=fs,subdomain=fs2 → fs2.<domain> 命中,fs.<domain> 不命中。
}

#[tokio::test]
async fn explicit_route_overrides_auto() {
    // 同一 subdomain,显式 route to=别处 → 显式胜出。
}

#[tokio::test]
async fn wildcard_matches_single_label() {
    // route host=*.opc.dongx.site → a.opc.dongx.site 命中,a.b.opc.dongx.site 不中。
}

#[tokio::test]
async fn upstream_receives_forwarded_headers() {
    // 上游回显收到的头 → 客户端断言 X-Forwarded-For/Proto/Host 正确,Connection 已剥。
    // (Task 9 的 echo_upstream 升级为回显头版本)
}
```

**Step 2: Run — 跑全部 e2e**

Run: `cargo test --jobs 6 --features reverse-proxy --test proxy_e2e`
Expected: 多数 PASS;失败的用例暴露实现缺陷(预期可能:SSE 时序——若实现误用了缓冲;503 页面——若 AutoService 分支未查状态)

**Step 3: Green — 修实现直到全绿**

逐个修失败用例。每修一个跑一次该用例。**不要在多个用例失败时一次性大改实现**——逐个定位。

**Step 4: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 5: Commit**

```bash
git add tests/proxy_e2e.rs src/proxy/
git commit -m "test(proxy): P1 e2e 补全(SSE/大文件/421/502/503/auto/通配/头透传)"
```

---

### Task 11:WebSocket 隧道透传

**Files:**
- Modify: `src/proxy/forward.rs`(加 WS 升级分支)
- Modify: `Cargo.toml`(dev-dep: axum `ws` feature,e2e 用)
- Test: `tests/proxy_e2e.rs`(加 WS 用例)

**Step 1: Red — WS e2e**

`Cargo.toml` `[dev-dependencies]`:`axum = { version = "0.8", features = ["ws"] }`(注意 dev-dep 与 dep 分离,e2e 用 ws feature)

`tests/proxy_e2e.rs` 加:

```rust
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};

async fn ws_echo_upstream() -> SocketAddr {
    let app = Router::new().route("/ws", get(|ws: WebSocketUpgrade| async move {
        ws.on_upgrade(|socket: WebSocket| async move {
            // 收一条回一条
            let (mut tx, mut rx) = socket.split();
            while let Some(Ok(msg)) = rx.next().await {
                let _ = tx.send(msg).await;
            }
        })
    }));
    // ... bind 随机端口,返回 addr
}

#[tokio::test]
async fn proxies_websocket_echo() {
    let ws_addr = ws_echo_upstream().await;
    // [proxy] 路由 fs.opc.dongx.site → ws_addr
    // 客户端:用 tokio-tungstenite 连 ws://proxy_addr/ws,带 host 头
    // 发 "hello",收一条,断言 == "hello"
    // 再发 "world",收一条,断言 == "world"
    // 断开传播:客户端断开 → 上游 task 结束
}
```

> 注:tokio-tungstenite 不是 warden 既有依赖。**两个选择**:(a) 加 tokio-tungstenite dev-dep;(b) 用 axum 的 client 端 ws(若 axum 提供)。推荐 (a),dev-dep 不影响发布。实现时确定。

**Step 2: Run — 确认失败**

Run: `cargo test --jobs 6 --features reverse-proxy --test proxy_e2e proxies_websocket_echo`
Expected: 失败(WS 未透传,上游收不到握手或 421)

**Step 3: Green — 加 WS 隧道分支**

`src/proxy/forward.rs` `proxy_handler` 内,在常规转发之前检测:
- 请求头含 `Upgrade: websocket`(不区分大小写)且 `Connection: upgrade` → 走隧道分支
- 隧道:不经连接池,对上游新建专用连接(hyper-util client 的 `ready()` + `send_request` 拿到 upgrade future);`hyper::upgrade::on(req)` 拿客户端侧升级流;`tokio::io::copy_bidirectional` 双向复制
- 隧道无超时;shutdown 时随 drain 上限强关

> 关键实现细节:axum 0.8 的 `Request` body 是 `Incoming`,upgrade 需要把 request 整个转给 hyper client。参考 hyper-util legacy client 的 upgrade 范式(`client.send_request(req)` 后 `Response::extensions().get::<OnUpgrade>()`)。

**Step 4: Run — WS e2e 通过**

Run: `cargo test --jobs 6 --features reverse-proxy --test proxy_e2e proxies_websocket_echo`
Expected: PASS

**Step 5: Run — 双 feature 三项全绿**

Run: fmt + clippy + test(双 feature)
Expected: 全绿

**Step 6: Commit**

```bash
git add Cargo.toml src/proxy/forward.rs tests/proxy_e2e.rs
git commit -m "feat(proxy): P1 WebSocket 隧道透传(copy_bidirectional 双向复制)"
```

---

### Task 12:P1 收尾 —— CI feature job + desktop 退 default-features + ROADMAP

**Files:**
- Modify: `.github/workflows/ci.yml`(加 `--features reverse-proxy` job)
- Modify: `desktop/src-tauri/Cargo.toml`(warden 依赖加 `default-features = false`)
- Modify: `docs/ROADMAP.md`(P1 checkbox 勾上 + 变更日志)

**Step 1: CI 加 feature job**

`.github/workflows/ci.yml` 在既有 test step 旁加一个 matrix 变体或新 step:`cargo test --jobs 6 --features reverse-proxy`(windows + ubuntu 各一,对齐既有矩阵)。

**Step 2: desktop 退 default-features**

`desktop/src-tauri/Cargo.toml` 的 warden path 依赖:

```toml
warden = { path = "../..", default-features = false }
```

> 验证:desktop crate `cargo check`(在 desktop/src-tauri 下)仍通过——desktop 不需要反代,且其代码不引用 `warden::proxy`。

**Step 3: ROADMAP 更新**

`docs/ROADMAP.md` Phase 6:
- `[ ] P1 HTTP 反代 MVP...` 改 `[x] P1 HTTP 反代 MVP ✅(2026-09-18)`
- 变更日志加一条,记录:P1 完成,路由(精确/通配/auto)+ hyper-util 流式直传 + WebSocket 隧道 + 421/502/503 错误页,e2e 覆盖 SSE/大文件/WS/auto/通配/头透传;CI 加 feature job;desktop 退 default-features。

**Step 4: Run — 全量双 feature 三项 + desktop check**

Run:
```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --jobs 6
cargo test --jobs 6 --features reverse-proxy
cd desktop/src-tauri && cargo check --jobs 6
```
Expected: 全绿(desktop 不引用 proxy,check 通过)

**Step 5: Commit**

```bash
git add .github/workflows/ci.yml desktop/src-tauri/Cargo.toml docs/ROADMAP.md
git commit -m "chore(proxy): P1 收尾——CI feature job + desktop 退 default-features + ROADMAP"
```

---

## P1 完成验收清单

- [ ] `cargo test --jobs 6` 无 feature 全绿(既有 96 + 1 ignored + P0 新增,无回归)
- [ ] `cargo test --jobs 6 --features reverse-proxy` 全绿(P0 + P1 新增约 25-35 个)
- [ ] `cargo clippy --all-targets -- -D warnings` 双 feature 干净
- [ ] `cargo fmt --all --check` 干净
- [ ] desktop crate `cargo check` 通过(default-features=false)
- [ ] e2e 覆盖:body 透传 + XFF 头、SSE 流式时序、大文件 8MiB 哈希、421、502、503(auto 服务停止)、auto subdomain 覆盖、显式覆盖 auto、通配单层、WebSocket echo、上游收到转发头
- [ ] `[proxy]` 配置在 services.example.toml 有注释示例
- [ ] ROADMAP Phase 6 P0/P1 勾选 + 变更日志
- [ ] 真实服务联调(可选,记录于 ROADMAP 或 TESTING 文档):argus 或 safe_bot 经反代访问,观察 UI/SSE/大图上传

## 风险与未覆盖点(交接给 P2/P3)

- **P1 不含 TLS**:本机测试用 `http_bind`(8080),HTTPS 留 P2。真实带证书的访问在 P2 落地后验证。
- **auto 路由的服务状态查询**:依赖 Supervisor snapshot 的 `proxy=true` 服务列表与状态——若 Supervisor 未暴露便捷查询方法,P1 需补一个(对齐既有 `snapshot_status` 风格)。
- **WS e2e 的 dev-dep**:tokio-tungstenite 或等价;若加 dev-dep 要确认不污染发布构建。
- **SSE 时序测试的 flaky 风险**:时序断言(< 300ms)在 CI 慢机器上可能 flaky;若出现,放宽阈值或改用"收到顺序 + 非一次性到达"的弱断言(对齐仓库"测试要验证意图"——意图是"不缓冲",非"精确时序")。
- **proxy drain 15s 与既有 API drain 5s 并存**:`serve_with_shutdown` 需同时管理两组 drain;shutdown 触发后两组并行等待,总退出时间 = max(5s, 15s)。文档已记。

---

## 执行交接

计划已保存至 `docs/PLAN-REVERSE-PROXY-P0P1.md`。两种执行方式:

1. **Subagent-Driven(本会话)**:我按 Task 顺序派发 fresh subagent 逐个执行,每个 Task 之间做 code review,快速迭代。
2. **Parallel Session(新会话)**:你开新会话,用 `executing-plans` 技能批量执行,带 checkpoint。

**选哪种?**
