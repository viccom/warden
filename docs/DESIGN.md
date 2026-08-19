# warden 设计文档

> 本文档记录 warden 的架构、数据模型、API 与关键决策,是开发的权威参考。
> 进度与分 Phase 路线图见 [`ROADMAP.md`](./ROADMAP.md)。后续会话先读 ROADMAP 进度,再回本文档查设计。

## 1. 项目定位

`warden` 是一个 **Rust 进程监护管理工具**(supervisord / pm2 风格的 supervisor daemon),用于在边缘端统一拉起、监护、监测一组本地进程。

- **形态**:一个 daemon 进程,内含监护引擎 + HTTP API server。所有能力通过本地 HTTP API 暴露,**API 契约先行**;TUI(Phase 3)与 Web 前端(Phase 4)都连同一 API。
- **管理模型**:自带监护 —— daemon 自己 `spawn` 子进程、接管 stdout/stderr、监听退出、按策略决定是否重启。不依赖被管理程序是"服务型程序"(区别于 serviceMgr-tui 的 OS 原生服务注册模式)。
- **daemon 自身存活**:Phase 2 通过自注册成 Windows Service / systemd 服务实现开机自启与后台常驻(此时反向复用 OS 服务注册能力注册它自己)。Phase 1 daemon 仅前台运行(`warden run`)。
- **平台**:Windows 优先(LTSC 2021 现场),代码用 `cfg(target_os)` 抽象,Linux/macOS 可编译。
- **归属**:独立通用工具。`config/services.example.toml` 预置 rs-iot 三件套作为典型用例。

## 2. 架构形态

```
┌─────────────────── warden daemon ───────────────────┐
│                                                      │
│  ┌──────────────┐         ┌────────────────────┐     │
│  │  Supervisor  │  共享   │   HTTP API (axum)  │     │
│  │  监护引擎    │←───────→│   :8789 (本地)     │     │
│  │              │  状态   │                    │     │
│  │ • 状态机     │         │  services CRUD     │     │
│  │ • spawn/wait │         │  start/stop/restart│     │
│  │ • restart/   │         │  logs (SSE 流)     │     │
│  │   backoff    │         │  metrics           │     │
│  │ • LogHub     │         │  health            │     │
│  │ • metrics    │         └─────────┬──────────┘     │
│  └──────┬───────┘                   │                │
│         │ tokio::process::Command::spawn             │
│    ┌────┴────────┬───────────┬───────────┐           │
│    ▼             ▼           ▼           ▼           │
│  子进程A      子进程B      子进程C    (任意进程)      │
│  (rs-iot)   (reasonix)   (gateway)                   │
│                                                      │
│  config.toml 加载 + 热重载   tracing 多 layer        │
│  graceful shutdown (CancellationToken)               │
└──────────────────────────────────────────────────────┘
              ↑ HTTP 127.0.0.1:8789 (静态 token)
        curl / 外部程序
        TUI (Phase 3) / Web (Phase 4) 后续接入
```

**数据流**:`config.toml` → `Vec<ServiceConfig>` → Supervisor 为每个服务建 `ProcRuntime`(状态 + LogHub + 句柄)→ 用户/外部经 HTTP 触发 start/stop/restart → Supervisor 操作子进程、状态机流转 → API 查询返回实时状态/日志/指标。

## 3. 技术栈

与 rs-iot 同版本栈,便于统一维护与代码风格延续。

| 用途 | crate | 版本 |
|---|---|---|
| async runtime | tokio | 1 (full) |
| HTTP server | axum | 0.8 (macros) |
| middleware | tower / tower-http | 0.5 / 0.6 (trace, cors) |
| 序列化 | serde / serde_json / toml | 1 / 1 / 0.8 |
| 日志 | tracing / tracing-subscriber / tracing-appender | 0.1 / 0.3 (env-filter) / 0.2 |
| 错误 | thiserror (库) + anyhow (bin) | 2 / 1 |
| CLI | clap | 4 (derive) |
| 流式 | tokio-stream / futures-util / bytes | 0.1 / 0.3 / 1 |
| 并发表 | dashmap | 6 |
| 资源采集 | sysinfo | 0.32 |
| 时间 | chrono | 0.4 (serde) |
| 其他 | async-trait / directories | 0.1 / 5 |

Phase 1 **不引入**:ratatui/crossterm (P3 TUI)、windows-service (P2 OS 注册)、encoding_rs (P2 Windows 编码)、reqwest (P3 TUI 客户端)。

edition 2021 / rust-version 1.81。

## 4. 目录结构

```
warden/
├── Cargo.toml                  单 crate(lib + bin)
├── rust-toolchain.toml         stable + rustfmt + clippy
├── .gitignore
├── docs/
│   ├── DESIGN.md               本文档(架构/模型/API/决策)
│   └── ROADMAP.md              分 Phase 路线图 + 进度 checklist
├── config/services.example.toml   rs-iot 三件套预置示例
├── src/
│   ├── main.rs                 clap CLI(run 本次实现;tui/install/service 占位)
│   ├── lib.rs                  库 re-export(便于集成测试)
│   ├── config.rs               toml 配置 + Default + 路径查找 + 校验
│   ├── error.rs                thiserror WardenError + impl IntoResponse
│   ├── model.rs                ServiceConfig/ProcState/ProcRuntime/RestartPolicy/HealthCheck
│   ├── logs.rs                 LogHub(VecDeque 环缓冲 + broadcast + 文件轮转)
│   ├── supervisor/
│   │   ├── mod.rs              Supervisor 引擎(DashMap<name, ProcRuntime>)
│   │   ├── proc.rs             单进程 spawn/wait/restart/backoff 状态机
│   │   └── metrics.rs          sysinfo 周期采样
│   └── api/
│       ├── mod.rs              build_router + AppState + token 中间件
│       ├── auth.rs             静态 token 鉴权(Header Bearer)
│       ├── routes_service.rs   services 列表/详情/start/stop/restart/reload/all
│       ├── routes_logs.rs      logs 快照 + SSE 流
│       └── routes_health.rs    daemon 健康
└── tests/
    ├── common/mod.rs           测试 harness(spawn 真被监护进程用 sleep/ping)
    ├── api_flow.rs             API 集成测试(tower oneshot)
    └── supervisor_e2e.rs       监护引擎 e2e
```

## 5. 核心数据模型(`model.rs`)

### 5.1 配置层(从 toml 反序列化,持久于配置文件)

```rust
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ServiceConfig {
    pub name: String,                            // 唯一标识
    #[serde(default)] pub display_name: String,
    #[serde(default)] pub description: String,
    pub command: String,                         // 可执行文件路径
    #[serde(default)] pub args: Vec<String>,     // 参数数组(优于 serviceMgr-tui 单字符串)
    #[serde(default)] pub working_dir: Option<String>,
    #[serde(default)] pub environment: HashMap<String, String>,
    #[serde(default)] pub auto_start: bool,      // daemon 启动时拉起
    #[serde(default)] pub auto_restart: bool,    // 崩溃自动重启,默认 false
    #[serde(default)] pub restart: RestartPolicy,
    #[serde(default)] pub health: Option<HealthCheck>,
    #[serde(default)] pub ui_url: Option<String>,        // 管理入口,UI「打开」按钮
    #[serde(default)] pub config_file: Option<String>,   // 子进程配置文件,UI「编辑」入口
    #[serde(default = "default_graceful_timeout_secs")] pub graceful_timeout_secs: u64, // 默认 10
    #[serde(default)] pub output_encoding: Option<String>, // "gbk"/"cp936"/"utf-8";None=UTF-8
    #[serde(default)] pub group: Option<String>,   // 分组标签(纯展示,不参与排序)
    #[serde(default)] pub priority: u32,           // 启动优先级:小者先启动、越后停止;同值按 name 字典序
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RestartPolicy {
    #[serde(default)] pub mode: RestartMode,          // 默认 Always
    #[serde(default = "default_expected_exit_codes")] pub expected_exit_codes: Vec<i32>, // 默认 [0]
    pub max_retries: u32,            // 默认 3
    pub backoff_initial_ms: u64,     // 默认 1000
    pub backoff_max_ms: u64,         // 默认 60000
    pub backoff_factor: f64,         // 默认 2.0
    pub restart_window_secs: u64,    // 默认 60:窗口内重启超 max_retries 才算 Failed
}
// mode: Always(任意退出都重启,默认)/ Unexpected(退出码在 expected_exit_codes 内 →
//   Stopped 不重启,子进程自升级场景;被信号杀死记 -1 哨兵,需覆盖配 -1)/
//   Never(任意退出不重启);auto_restart=false 时一律按 Never 生效。
// expected_exit_codes 留空 = 任何退出码都不命中白名单,退化为退避重启,不建议。
// impl Default 给上述默认值

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HealthCheck {
    Tcp { host: String, port: u16, timeout_ms: u64, interval_secs: u64 },
    // timeout_ms 默认 2000,interval_secs 默认 5
}
```

### 5.2 运行态(内存,不持久化)

```rust
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum ProcState {
    Stopped,
    Starting,
    Running { pid: u32, started_at: DateTime<Utc> },
    Stopping,
    Failed { reason: String, exit_code: Option<i32>, at: DateTime<Utc> },
    Restarting { attempt: u32, next_at: DateTime<Utc> },
}

pub struct ProcRuntime {
    pub config: ServiceConfig,
    pub state: ProcState,           // RwLock 保护,API 读快照
    pub child: Option<ChildHandle>, // tokio::process::Child
    pub restart_count: u32,
    pub last_started_at: Option<DateTime<Utc>>,
    pub log: Arc<LogHub>,           // 该服务的日志收集器
    pub metrics: Option<ProcMetrics>,
}

pub struct ProcMetrics {
    pub cpu_percent: f32,
    pub memory_kb: u64,
    pub sampled_at: DateTime<Utc>,
}
```

## 6. 监护状态机(`supervisor/`)

每个被监护服务在 Supervisor 里对应一个或一组后台 task。

```
                start()
   Stopped ──────────────► Starting ──spawn ok──► Running {pid, started_at}
     ▲                        │                        │
     │                  spawn fail                     │ child exit
     │                        ▼                        ▼
     │                      Failed    ┌──── auto_restart=false ────► Failed (exit!=0)
     │                                │
     │                                │   ┌── mode=unexpected 且 exit ∈ expected_exit_codes ──► Stopped(预期退出,不重启)
     │                                │   │
     │                                │   ┌── attempt < max ──► Restarting {attempt,next_at}
     │                                └───┤                    │ sleep(backoff)
     │                                    │                    ▼
     │                                    │                 Starting
     │                                    │
     │                                    └── attempt >= max(窗口内) ──► Failed
     │
     │  stop()
     └──── Stopping ◄───────────────────────── Running
                       kill ok
```

**关键行为**:
- **spawn**:`tokio::process::Command::new(command).args(args).envs(env).current_dir(working_dir).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()`。
- **spawn 失败零重试**:命令不存在/无权限等 spawn 即错 → 直接 `Failed{reason: "spawn 失败:…"}`,不进 backoff 重启决策(区别于进程退出路径;不采用 supervisord 的 FATAL 重试——坏路径重试无意义且徒刷日志)。
- **退出处理**:wait task `child.wait().await` → 据 `exit_code` + 生效模式决策(`auto_restart=false` 强制按 `Never`,否则用 `restart.mode`)。`restart_window`:距上次启动超过窗口则重置计数(避免长期运行的服务偶尔崩溃也被计入熔断)。被信号杀死 `exit_code=None`,白名单匹配记 `-1` 哨兵。
- **backoff**:第 n 次重试等待 `min(initial * factor^(n-1), max)`。例:1000ms → 2000 → 4000 → 8000 … 封顶 60000ms。
- **stop(Phase 1)**:`child.kill()`(Windows = TerminateProcess,强制终止)。优雅停止(Linux SIGTERM / Windows GenerateConsoleCtrlEvent / Job Object)列 **Phase 4**。
- **日志接管**:spawn 后起两个 reader task,按行读 stdout/stderr → 推入该服务 `LogHub`(区分 stdout/stderr 标记)。
- **metrics**:sysinfo 每 2s 按已记录的 PID 采 CPU/内存,写入 `ProcRuntime.metrics`。
- **有序启停**:`start_all`/`start_auto` 按 priority 升序(name 字典序 tie-break)启动 + **就绪推进**(每服务等到离开 Starting、上限 15s 再启动下一个,Failed/Restarting 不阻塞后续);`stop_all` 逆序(被依赖方最后停)。见 `Supervisor::ordered_names`/`start_ordered`。
- **监听端口发现**(`supervisor/ports.rs`):metrics task 周期采集全系统 socket 表(netstat2,Windows GetExtendedTcp/UdpTable / Linux netlink)→ 按**服务 PID 子树**(含孙进程,启动器形态)过滤 TCP LISTEN / UDP 绑定 → 写入 `ProcInner.ports`,经 `ServiceStatus.listening_ports` 透出。UDP 仅"已绑定"语义(无 listen),不支持 UDP 健康检查。

**并发模型**:Supervisor 持 `DashMap<String, ProcRuntime>`。状态读写用 `RwLock`/`Mutex` 保护最小临界区;长操作(spawn/wait/backoff sleep)在独立 tokio task,不阻塞 API 线程。这与 rs-iot 的 InstanceManager(states/handles/configs 多 DashMap)模式一致。

## 7. 日志收集(`logs.rs` —— LogHub)

每个服务一个 `LogHub`,提供历史回看 + 实时订阅 + 文件落盘三路:

```rust
pub struct LogLine {
    pub stream: LogStream,         // Stdout | Stderr
    pub ts: DateTime<Utc>,
    pub text: String,
}

pub struct LogHub {
    history: Mutex<VecDeque<LogLine>>,   // 容量 2000,满弹头
    tx: broadcast::Sender<LogLine>,      // 实时订阅(容量 256,慢消费者丢历史)
    file: Mutex<Option<RollingWriter>>,  // data_dir/logs/<name>/<date>.log,按日轮转
}
```

- **历史快照**:`snapshot(tail_n)` 从 VecDeque 取最近 N 行(供 `GET /logs?tail=N`)。
- **实时流**:`subscribe()` 返回 broadcast Receiver(供 `GET /logs/stream` 的 SSE 推送)。
- **文件落盘**:每行同步 append 到 `data_dir/logs/<name>/<YYYY-MM-DD>.log`(tracing-appender daily 风格)。Phase 1 落盘可选(`log_dir` 非空才写)。

## 8. HTTP API

路由前缀 `/api/v1`。handler 返 `Result<Json<T>, WardenError>`,由 `IntoResponse` 统一转错误响应。

| Method | Path | 说明 |
|---|---|---|
| GET | `/` | 内置 Web UI 单页(状态/日志/CRUD/配置编辑) |
| GET | `/api/v1/health` | daemon 健康(版本、服务数、running/failed 计数、`[daemon] title`) |
| GET | `/api/v1/services` | 列出全部(配置 + 状态 + metrics 摘要) |
| POST | `/api/v1/services` | 新增服务(校验后写回配置文件,重名/运行中 409) |
| GET | `/api/v1/services/{name}` | 单服务详情 |
| PUT | `/api/v1/services/{name}` | 修改服务(写回配置文件;运行中保存下次启动生效) |
| DELETE | `/api/v1/services/{name}` | 删除服务(运行中 409) |
| POST | `/api/v1/services/{name}/start` | 启动 |
| POST | `/api/v1/services/{name}/stop` | 停止(优雅:信号 → graceful_timeout → 强杀进程树) |
| POST | `/api/v1/services/{name}/restart` | 重启(stop → start) |
| GET | `/api/v1/services/{name}/logs?tail=500` | 日志快照 |
| GET | `/api/v1/services/{name}/logs/stream` | SSE 实时流 |
| GET | `/api/v1/services/{name}/metrics` | CPU/内存/PID/uptime/监听端口 |
| GET | `/api/v1/services/{name}/config` | 服务配置(编辑表单预填用) |
| GET/PUT | `/api/v1/services/{name}/config-file` | 子进程配置文件读写(toml/json 校验+格式化) |
| POST | `/api/v1/services/start-all` | 启动全部(按 priority 有序) |
| POST | `/api/v1/services/stop-all` | 停止全部(逆序优雅) |
| POST | `/api/v1/groups/{group}/start` | 组内全部启动(按 priority) |
| POST | `/api/v1/groups/{group}/stop` | 组内全部停止(逆序) |
| POST | `/api/v1/config/reload` | 重新加载配置文件(diff 应用,消失的服务优雅移除) |

- **鉴权(Phase 1)**:静态 token。`config.daemon.auth_token` 非空时校验 `Authorization: Bearer <token>`;`/api/v1/health` 放白名单(便于探活)。监听 `127.0.0.1`。Phase 4 加 Web 时再升级为 JWT + login。
- **graceful shutdown**:`tokio_util::sync::CancellationToken` 监听 ctrl_c → `cancel()` → 停所有子进程 + 关 API(对齐 rs-iot `lib.rs` 模式)。
- **SSE**:`axum::response::Sse` + `tokio_stream::wrappers::BroadcastStream`,把 LogHub 的 broadcast 转成 SSE 事件流(对齐 rs-iot `/api/v1/events`)。

## 9. 配置(`config.rs`)

```toml
[daemon]
api_bind      = "127.0.0.1:8789"
auth_token    = "warden-secret-change-me"   # 留空 "" 则不鉴权
data_dir      = "./data"
log_dir       = "./logs"
# alert_webhook = "http://127.0.0.1:9000/hook"   # 健康检查状态迁移告警(POST JSON)
# env = { RUST_LOG = "info" }                     # 全局环境变量(service 同名覆盖)
# title = "my-warden"                             # 桌面版窗口/标题栏名称(CLI 忽略)

[[service]]
name         = "..."
command      = "..."
args         = ["..."]
working_dir  = "..."
auto_start   = true
auto_restart = false
# 重启策略缺省走 RestartPolicy::default(),也可显式写:
# restart = { max_retries = 3, backoff_initial_ms = 1000, backoff_max_ms = 60000, backoff_factor = 2.0, restart_window_secs = 60 }
# 子进程自升级场景(旧进程 fork 后 exit 0 不当作崩溃)——dotted-key 多行写法:
# restart.mode = "unexpected"
# restart.expected_exit_codes = [0]
```

- **路径查找优先级**(CLI;桌面版另有独立链,见 `desktop/src-tauri/src/daemon.rs`):
  1. `$WARDEN_CONFIG` 环境变量
  2. `<exe_dir>/config/services.toml`
  3. `./config/services.toml`
  4. 平台标准位置(用户级,`directories` ProjectDirs;Windows `%APPDATA%\warden\config\services.toml`,Linux `~/.config/warden/services.toml`)
- **cwd 锚定**(`config_base_dir`,CLI 与桌面版同规则):daemon 启动时把进程 cwd 固定到配置基准目录——文件在 `<base>/config/` 下取 `<base>`,否则取文件所在目录。配置内相对路径(working_dir/command/data_dir/log_dir)与启动方式解耦:从任意目录 `warden run`、Service 模式(cwd=System32)、桌面版任意 exe 位置,解析结果一致;整目录迁移无需改配置。
- **校验**:name 非空、唯一、字符白名单(禁 `/\:*?"<>|`);command 非空(不预检路径——spawn 失败零重试进 Failed,见 §6);group 不含 `/`(组级 API 路由参数)。坏项**跳过并 warn**(对齐 serviceMgr-tui parser 容错,不因一个坏服务拖垮整体加载)。
- **Default**:`#[serde(default)]` + 手写 `impl Default`,partial 配置安全(对齐 rs-iot `config.rs`)。

## 10. 错误处理(对齐 rs-iot,分层)

- **库层**:`thiserror` enum `WardenError`,`#[from]` 收纳 `io::Error` / `toml::de::Error` / `serde_json::Error` 等,加语义变体(`Config(String)` / `ServiceNotFound` / `InvalidState`)。
- **binary 层**:`anyhow::Result`(main)。
- **HTTP 边界**:`impl IntoResponse for WardenError` → 返 `{ "error": "<variant>", "message": "<msg>" }` + 合适状态码(NOT_FOUND / CONFLICT / INTERNAL_SERVER_ERROR)。**改进点**:rs-iot 没做 handler 级 `Result + IntoResponse`,warden 补上,handler 可写 `Result<Json<T>, WardenError>`。

## 11. 测试策略(对齐 rs-iot 两层)

- **内联单测**(`#[cfg(test)] mod tests`,在 src 文件内):
  - config 解析 / Default 契约 / 坏项跳过
  - backoff 计算(`initial*factor^(n-1)` 封顶 max)
  - 状态机转换合法性
  - 路径查找优先级
- **集成测试**(`tests/`):
  - `common/mod.rs`:helper 构造 Supervisor + 用跨平台无害命令(`cmd /c ping -t localhost` / `sleep 60` / `ping`)做被监护进程。
  - `supervisor_e2e.rs`:spawn sleep → Running → stop → Stopped;spawn 速退(`cmd /c exit 1`)→ auto_restart=true → Restarting 计数递增 → 超 max_retries → Failed。
  - `api_flow.rs`:`tower::ServiceExt::oneshot` 打 `build_router(state)`:start → GET services 见 Running → stop → Stopped;logs 快照非空;health 200。

## 12. 关键决策与权衡

| 决策 | 选择 | 理由 |
|---|---|---|
| 架构模式 | 自带监护(supervisor)而非 OS 原生服务注册 | rs-iot 三件套(exe / node cmd shim)都不是服务型程序,自带监护可管任意可执行文件;OS 注册能力仅用于注册 daemon 自身(P2) |
| 崩溃重启 | 内置但默认关闭 | 默认不干扰现场调试;每服务可显式开 auto_restart + 配退避 |
| stop 方式 | 强制 kill(P1) | Windows 无对任意进程的优雅信号;TerminateProcess 够用;优雅停止列 P4 |
| 鉴权 | 静态 token(P1) | 无前端本地工具,JWT 太重;P4 加 Web 再升级 |
| 运行态持久化 | 不持久化;**配置文件是唯一数据源**(2026-08-17 重构) | CRUD 经 `config_edit`(toml_edit 保注释)直接写回配置文件;daemon 重启只按 auto_start 拉起(supervisord 语义)。曾有的 runtime overlay + desired_state 已废除,启动时一次性迁移(旧文件改 .bak)——单数据源,删文件即清空,无"幽灵服务" |
| handler 错误 | `Result + IntoResponse` | 比 rs-iot 手写 `Json<Value>` 规整,新项目做改进(规则 6 暴露而非折中) |
| 单 crate(lib+bin) | MVP 不拆 workspace | 简洁优先;P3 加 TUI 再评估拆 `warden-tui` crate |
| TUI/Web 接入 | 连 HTTP API | API 契约先行,前端形态可换;TUI 用 reqwest 连本地/远程 API |

## 13. 参考项目

- **serviceMgr-tui**(`D:\Go_Codes\serviceMgr-tui`,Go):OS 原生服务注册 + TUI。借鉴:配置路径查找优先级、坏项跳过容错、服务名字符校验、退出码翻译、UAC 提权思路(P2)、暗色 TUI 交互范式(P3)。
- **rs-iot**(`E:\github.com\rs-iot`,Rust):技术栈与代码风格来源。借鉴:workspace/profile、clap + toml::from_str + serde Default、tracing 多 layer + appender guard、axum build_router + 中间件、rust-embed SPA fallback(P4 Web)、CancellationToken graceful shutdown、tower oneshot 集成测试范式、thiserror+anyhow 分层。
