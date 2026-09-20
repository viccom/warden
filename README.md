# warden

[![CI](https://github.com/viccom/warden/actions/workflows/ci.yml/badge.svg)](https://github.com/viccom/warden/actions/workflows/ci.yml)
[![Release](https://github.com/viccom/warden/actions/workflows/release.yml/badge.svg)](https://github.com/viccom/warden/actions/workflows/release.yml)

Rust 进程监护管理工具(supervisord / pm2 风格的 supervisor daemon):统一拉起、监护、监测一组本地进程,提供 CLI、TUI、Web 与桌面客户端四种形态。

```
┌──────────────────────────────────────────────────────────┐
│ warden lib(监护引擎 + HTTP API + 域名反向代理)             │
│  spawn/优雅停止 · 日志管道 · 重启策略 · 健康检查             │
│  分组/优先级 · 端口发现 · metrics · CRUD                   │
│  反代:按子域路由 · TLS 终止/热重载 · WS/SSE 直传            │
├────────┬────────┬────────┬───────────────────────────────┤
│ CLI run│  TUI   │ Web UI │ 桌面版(Tauri 2)                │
│ 服务模式│ 终端    │ 浏览器  │ 多节点 + 托盘                  │
└────────┴────────┴────────┴───────────────────────────────┘
```

## 特性

- **自带监护**:daemon 直接 spawn 子进程并接管 stdout/stderr,不要求被管理程序是"服务型程序"
- **优雅停止**:Windows `CTRL_BREAK` 信号(独立进程组精确投递)+ 超时强杀 + Job Object 杀整棵进程树;`graceful_timeout_secs` 可按服务配置
- **崩溃重启策略**:指数退避 + 重试上限 + 计数重置窗口(可演示见 `config/services.crash.toml`)
- **健康检查**:TCP 探测(间隔/超时可配),状态迁移写日志 + 可选 webhook 告警
- **分组与启动优先级**:数值越小越先启动、越后停止(对齐 supervisord 语义),支持组级批量启停
- **端口发现**:自动枚举被监护进程(含孙进程)的 TCP/UDP 监听端口
- **GBK 输出解码**:中文 Windows 控制台程序输出按 `output_encoding = "gbk"` 正确解码
- **运行时 CRUD**:HTTP API 增删改服务,直接写回 TOML 配置文件(唯一数据源,保留注释),重启后原样恢复
- **反向代理**(`reverse-proxy` feature,默认开启;desktop 形态不含):基于域名(通配单层)的 L7 反代,把被监护的 Web 服务按子域对外暴露——TLS 终止(单张通配证书,mtime 轮询热重载)+ http→https 301 + WebSocket/SSE/大文件流式直传 + 显式路由与「auto 服务暴露」(`proxy=true` 的服务按 `subdomain` 自动获得 `*.domain` 子域) + 运行时路由 CRUD(热生效,免重启) + 按路由聚合的 metrics + 访问日志按日轮转 + Web 反代管理页(顶栏站点直达导航);证书到期检测告警 + 可选外部续期命令(与 acme.sh/lego 等任意 ACME 客户端协同;典型部署为 nginx/OpenResty/caddy 等主流 web 服务器前置终止 TLS、warden 二级按子域分流,流程见 `docs/TESTING-ACME.md`;证书编排器已立项见 `docs/PLAN-CERT-ORCHESTRATOR.md`)
- **鉴权**:`auth_token` Bearer 认证;桌面版内嵌 daemon 用随机端口 + 随机 token
- **桌面版**(Tauri 2 + Vue 3):多节点管理(本机内嵌 + 远程 warden)、自定义标题栏、系统托盘、配置文件在线编辑器、浅色/深色主题

## 快速开始

```bash
cargo build --release
./target/release/warden run --config config/services.example.toml

# HTTP API(默认 127.0.0.1:8789,auth_token 空则不鉴权)
curl http://127.0.0.1:8789/api/v1/health
curl http://127.0.0.1:8789/api/v1/services
curl -X POST http://127.0.0.1:8789/api/v1/services/<name>/start
```

配置文件是唯一数据源:

```toml
[daemon]
api_bind   = "127.0.0.1:8789"
auth_token = ""                  # 留空则不鉴权

[[service]]
name         = "my-app"
command      = "bin/my-app.exe"
args         = ["--config", "config/app.toml"]
working_dir  = "."
auto_start   = false
auto_restart = true
restart      = { mode = "always", max_retries = 3, backoff_initial_ms = 1000, backoff_max_ms = 60000, backoff_factor = 2.0, restart_window_secs = 60 }
# mode:"always"(默认)|"unexpected"(退出码在 expected_exit_codes 内视作预期退出,不重启——子进程自升级场景)|"never"
# unexpected 模式示例:restart.mode = "unexpected" / restart.expected_exit_codes = [0](默认 [0])
health       = { type = "tcp", host = "127.0.0.1", port = 8080, timeout_ms = 2000, interval_secs = 5 }
graceful_timeout_secs = 10
group        = "core"
priority     = 1
```

完整字段见 `config/services.example.toml`;演示配置见 `config/services.*.toml`。

## 形态

| 形态 | 命令 | 说明 |
|---|---|---|
| 前台 daemon | `warden run` | 主入口,监护引擎 + HTTP API |
| TUI | `warden tui` | 终端客户端(服务表格 + 实时日志) |
| Web UI | 浏览器开 `http://127.0.0.1:8789/` | 内置单页(状态/日志/CRUD/配置编辑/反向代理管理/站点直达) |
| 桌面版 | `desktop/` 构建 | Tauri 2 多节点客户端,见 [`desktop/README.md`](./desktop/README.md) |
| OS 服务 | `warden install / service` | Windows Service 自注册(开机自启) |

## 开发

```bash
cargo test                     # 全测(单测 + 集成;默认形态 173 个 + 无反代形态 123 个,`cargo test --no-default-features`)
cargo clippy --all-targets -- -D warnings
cargo fmt --all

# 桌面版(必须走官方命令,裸 cargo build 缺 custom-protocol 特性会白屏)
cd desktop && pnpm install && pnpm tauri build --no-bundle
```

## 文档

- [`docs/DESIGN.md`](./docs/DESIGN.md) —— 架构、数据模型、API 与关键决策(权威参考)
- [`docs/ROADMAP.md`](./docs/ROADMAP.md) —— 分 Phase 进度与变更日志
- [`docs/PLAN-REVERSE-PROXY.md`](./docs/PLAN-REVERSE-PROXY.md) —— 反向代理实施方案(决策 D1-D13,已实施)
- [`docs/PLAN-DESKTOP.md`](./docs/PLAN-DESKTOP.md) —— 桌面版实施方案
- [`docs/PLAN-GROUP-PRIORITY-PORTS.md`](./docs/PLAN-GROUP-PRIORITY-PORTS.md) —— 分组/优先级/端口发现实施方案(已实施)
- [`docs/PLAN-CERT-ORCHESTRATOR.md`](./docs/PLAN-CERT-ORCHESTRATOR.md) —— 证书编排器(warden 编排 lego,UI 一键申请/续期)实施方案(立项待实施)
- [`docs/TESTING-ACME.md`](./docs/TESTING-ACME.md) —— 证书外部托管协同指南(1Panel / acme.sh / lego 实战)
- [`docs/RESEARCH-SELF-UPDATE.md`](./docs/RESEARCH-SELF-UPDATE.md) —— 自升级技术调研(未实施)
- [`docs/TESTING-GBK.md`](./docs/TESTING-GBK.md) —— GBK 解码手动测试指南

## License

[MIT](./LICENSE)
