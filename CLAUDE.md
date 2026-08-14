# CLAUDE.md

This file provides guidance to Claude Code / ZCode when working with code in this repository.

## 项目地图

warden 是一个 **Rust 进程监护管理工具**(supervisord / pm2 风格的 supervisor daemon),在边缘端统一拉起、监护、监测一组本地进程。独立通用工具,典型用例是监护 rs-iot 三件套(rs-iot / reasonix serve / rsiot-gateway)。

**架构(自带监护 + daemon 自注册混合,见 `docs/DESIGN.md`)**:
- daemon 自己 `spawn` 子进程、接管 stdout/stderr、监听退出、按策略重启(**监护**)
- daemon 自身通过 OS 服务注册(Windows Service / systemd)开机自启(**Phase 2**,反向复用 serviceMgr-tui 的注册能力注册自己)
- 所有能力经本地 HTTP API 暴露,**API 契约先行**;TUI(Phase 3)/ Web(Phase 4)连同一 API

**单 crate(lib + bin)**:

| 模块 | 职责 |
|---|---|
| `main.rs` | clap CLI(`run` 前台跑 daemon;`tui`/`install`/`uninstall`/`service` 占位待 Phase 2/3) |
| `lib.rs` | 库入口;`run_app` 编排(config→tracing→auto_start→metrics→axum→graceful shutdown) |
| `config.rs` | toml 解析 + Default + 路径查找(`$WARDEN_CONFIG`→exe_dir→cwd→平台标准位置)+ 校验(坏项跳过并 warn) |
| `error.rs` | `WardenError`(thiserror + `#[non_exhaustive]` + `impl IntoResponse`)+ `WResult<T>` |
| `model.rs` | `ServiceConfig` / `ProcState` / `RestartPolicy` / `HealthCheck` / `ProcMetrics` |
| `logs.rs` | `LogHub`(VecDeque 环缓冲 2000 + broadcast 256 + 按日轮转文件) |
| `supervisor/` | 监护引擎:`mod`(Supervisor + ProcHandle + ServiceStatus)+ `proc`(状态机/backoff/spawn/wait)+ `metrics`(sysinfo 采样) |
| `api/` | axum `build_router` + token 鉴权中间件 + `routes_service`/`routes_logs`/`routes_health` + SSE |

**技术栈**(对齐 rs-iot 版本栈,便于统一维护):tokio 1 / axum 0.8 / serde+toml / thiserror+anyhow / clap / tracing(+appender)/ dashmap / sysinfo。edition 2021, rust-version 1.81。Phase 2 起:`windows-service` / `encoding_rs`;Phase 3:`ratatui` / `reqwest`;Phase 4:`rust-embed`。

**参考项目**:`D:\Go_Codes\serviceMgr-tui`(Go,OS 服务注册 + TUI 的蓝本)、`E:\github.com\rs-iot`(技术栈与代码风格来源)。

**跨会话接续**:`docs/ROADMAP.md`(进度 checklist + 下一步 + 已知局限 + 变更日志)是入口,新会话**先读它**;`docs/DESIGN.md` 是设计权威(架构/数据模型/API/状态机/决策)。

## 常用命令

```bash
cargo build                                  # debug 编译
cargo test                                   # 全测(单测 + 集成)
cargo test --test supervisor_e2e             # 单跑某集成测试
cargo clippy --all-targets -- -D warnings    # CI 门槛,零警告
cargo run -- run --config config/services.example.toml   # 前台跑 daemon

# HTTP API 默认 127.0.0.1:8789;auth_token 空(默认)则不鉴权
curl http://127.0.0.1:8789/api/v1/health
curl http://127.0.0.1:8789/api/v1/services
curl -X POST http://127.0.0.1:8789/api/v1/services/<name>/start
curl -X POST http://127.0.0.1:8789/api/v1/services/<name>/stop
curl "http://127.0.0.1:8789/api/v1/services/<name>/logs?tail=100"
```

## 编码约定

- **注释用中文**(与代码、文档一致),doc 注释完整(`//!`/`///`)。
- **错误处理**:库层 `thiserror` `WardenError`(`#[from]` 收纳底层错误 + `#[non_exhaustive]`)并 `impl IntoResponse`(handler 可返 `Result<Json<T>, WardenError>`,改进点:rs-iot 没做);binary 层(main)`anyhow`。
- **配置**:所有 struct `#[serde(default)]` + 手写 `impl Default`,partial 配置安全(对齐 rs-iot)。坏服务项**跳过并 warn**不致命(对齐 serviceMgr-tui 容错)。
- **异步生命周期**:监护 task 用 `CancellationToken` 退出,`tokio::select!` 监听 cancel 与 child.wait;stdout/stderr reader task 独立 spawn,EOF 自然退出。子进程句柄归监护 task 独占(避免锁内 await)。
- **handler 返回**:对齐 rs-iot 用 `Json<serde_json::Value>` + `json!` 宏(不强类型 DTO),但用 `WResult` 包错误统一转 HTTP。
- **跨平台**:子进程管理用 `tokio::process`(跨平台);OS 特异(Windows Service / 信号)用 `cfg(target_os)` mod 分发(Phase 2)。
- **`#[expect(...)]` 而非 `#[allow(...)]`**(对齐 rs-iot):lint 不再触发时编译器提示移除。测试跨 target 共享 helper 若某 target 不用,加 `#[allow(dead_code)]` + 注释(如 `tests/common::quick_fail`)。

## 验证规则

- 改 `src/` 任意代码:`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test` 三项全绿才算完成(与 CI 门槛一致)。
- 改监护逻辑(`supervisor/`):跑 `tests/supervisor_e2e.rs`(start/stop、速退 Failed、重启熔断、logs 捕获、metrics 采样)。
- 改 API(`api/`):跑 `tests/api_flow.rs`(`tower::ServiceExt::oneshot` 打 `build_router`,含鉴权拒绝/放行 + health 白名单)。
- 改配置语义(`config.rs`):跑 `config::tests`(解析/Default/坏项跳过/覆盖)。
- 新增逻辑补 `#[cfg(test)]` 内联单测或 `tests/` 集成测试。集成测试用 `tests/common`(`long_runner`/`quick_fail` 跨平台无害命令),**勿固定端口、勿写仓库 `./data/`/`./logs/`**。
- **已知局限(必读)**:`stop` 用 `TerminateProcess` **只杀直接子进程**;被监护程序若 spawn 子进程(如 `cmd /c reasonix → node`),孙子会残留孤儿(e2e 实测跑满 ping 时长)。Phase 4 用 Windows Job Object(KILL_ON_JOB_CLOSE)解决。**测试一律用直接进程(ping/sleep)避免孤儿**。

## 常见流程

- **新增 API 端点**:`api/routes_*.rs` 加 handler → `api/mod.rs` `build_router` 注册路由 → 免鉴权则在 `auth.rs` 加白名单。
- **新增配置字段**:`config.rs` 对应 struct 加 `#[serde(default)]` + `Default` 实现。
- **新增状态/监护行为**:`model.rs` 加状态 → `supervisor/proc.rs` 状态机分支。
- **Phase 推进**:读 `docs/ROADMAP.md` 对应 Phase → 实现 → 每步更新 ROADMAP checkbox + 变更日志 + 必要时补 DESIGN。

## 安全边界

- `target/`、`logs/`、`data/`、`*.log` 已 `.gitignore`,**不要提交**。
- `config/services.example.toml` 的 `auth_token` 是示例值,生产部署必须换;daemon 默认 `bind 127.0.0.1`(本地),远程暴露必须配非空 `auth_token`。
- 被 `stop` 的进程若 spawn 了子进程,孙子会孤儿(已知局限,见验证规则)。

## Skill 优先使用(Rust 开发)

写/改/审 Rust 代码前先调对应 skill 加载规范:

| 场景 | Skill |
|---|---|
| 写/改/审任何 Rust 代码 | `rust-best-practices` |
| 编译错误(所有权/借用/生命周期 E0382/E0597 等) | `m01-ownership` |
| 错误处理决策(`?`/unwrap/Result vs Option/thiserror vs anyhow) | `m06-error-handling` |
| Rust 疑问路由/选型/"怎么用/区别" | `rust-router` |
| 代码审查/PR review | `code-review-skill` |

与本文件冲突时,以本文件(项目实际约束)为准。

## 交付格式

每项任务完成按此格式汇报:
1. **改动总结**:改了什么模块、为什么。
2. **验证命令 + 结果**:实际跑过的 fmt/clippy/test 输出。
3. **风险与未覆盖点**:已知限制、未跑通的场景、需人工确认的配置。
