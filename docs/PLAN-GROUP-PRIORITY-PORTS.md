# 实施方案:服务分组与启动优先级 + 子进程监听端口发现

> **状态**:**已实施完成(2026-08-15)**,69 测试全绿 + clippy/fmt clean。checkbox 记录 TDD 过程;实施偏差见文末「实施记录」。
> **日期**:2026-08-15
> **关联**:[RESEARCH-SELF-UPDATE.md](./RESEARCH-SELF-UPDATE.md) —— 自升级的"有序 drain/恢复"复用需求 1 的排序能力,**需求 1 建议先行**。
> **验证门槛**(CLAUDE.md):每步 `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test` 三项全绿。

---

## 需求 1:分组 + 启动优先级

### 目标

- `[[service]]` 支持可选 `group`(分组标签)与 `priority`(启动优先级)。
- `start_all` / `start_auto` / `start_desired` 按优先级**顺序**启动;`stop_all` 按**逆序**停止。
- 状态快照(`ServiceStatus`)与 CRUD/API/Web 表单透出新字段。

### 范围外(本期不做,列为后续增强)

- 组屏障/健康门控("同组前序服务 Healthy 前不启动下一个")——见决策点 D4。
- 组级批量启停 API(UI 分组只是展示)。
- 服务间依赖图(`depends_on`)——priority 已覆盖主场景,不引入图模型。

### 设计

**配置 schema**(`src/model.rs` `ServiceConfig`,遵循 CLAUDE.md「新增配置字段」约定):

```toml
[[service]]
name     = "rs-iot"
group    = "core"    # 可选,自由字符串标签,默认无
priority = 10        # 可选,默认 0;数值越小越先启动、越后停止(对齐 supervisord 方向语义)
```

- 排序规则:**priority 升序 → 同 priority 按 name 字典序**(稳定、确定;现状是 DashMap 无序遍历,新配置即使全默认值也获得确定性顺序)。
- `group` 本期是**纯展示标签**:不参与排序(顺序全局由 priority 决定),不校验唯一性/预注册。
- 排序落点:服务名快照排序一次,复用给 start/stop:

```rust
// src/supervisor/mod.rs
/// 按启动顺序返回服务名(priority 升序,name 字典序);reverse=true 为停止顺序。
fn ordered_names(&self, reverse: bool) -> Vec<String>
```

`start_all` / `start_auto` / `start_desired` 用 `ordered_names(false)`,`stop_all` 用 `ordered_names(true)`。

**启动顺序的语义(关键取舍)**:当前 `start()` 只是 spawn 监护 task 后立即返回,子进程实际拉起是异步的;若只保证"发起顺序",实际进程启动顺序可能毫秒级交错。本方案采用**顺序 + 就绪推进 + 超时兜底**:

```
start_all 循环:start(n) → 等待 n 进入 Running / Failed / Restarting,或超过单服务等待上限(常量 15s)→ 启动下一个
```

- 满足真实意图(下游依赖上游先就绪,如 gateway 反代 rs-iot);
- 快速失败(quick_fail → Failed)与熔断重试中(Restarting,backoff 可能较长)不阻塞后续;
- 测试因此是确定性的(后发起的服务必然在前序进入终态/超时后才发起);
- 代价:`start-all` API 请求耗时变长(逐个等待就绪)。现状本就是串行 await,可接受。

### 决策点(待确认)

| # | 问题 | 方案默认 | 备选 |
|---|---|---|---|
| D1 | priority 方向 | **小值先启动、后停止**(supervisord 惯例;supervisord 默认 999 意为"垫底") | 大值先启动(直觉派)。已选 supervisord 方向,warden 是 supervisord 类工具,迁移心智一致 |
| D2 | priority 默认值 | `0`(全默认时按 name 字典序,确定性优于现状) | 999(supervisord 默认,新服务垫底) |
| D3 | 同优先级 tie-break | name 字典序 | 配置文件出现顺序(需改用 Vec 索引,DashMap 重建后语义模糊,弃) |
| D4 | 就绪推进的等待目标 | Running / Failed / Restarting / 15s 超时 | 弱语义(仅发起顺序,零等待):实现最简但测试 flaky、不满足依赖意图 |
| D5 | 组语义 | 纯展示标签 | 组内序号(`group` + 组内 priority)两级排序:复杂度不值,弃 |

### TDD 实施步骤

每步严格 Red(Git 仓可见失败测试)→ Green(最小实现)→ 按需重构。

- [x] **1. Red:配置解析单测**(`src/config.rs` `#[cfg(test)]`,对齐现有 `config::tests` 风格)
  - `parse_group_and_priority`:TOML 含 `group`/`priority` 正确读入。
  - `default_group_and_priority`:缺省为 `None` / `0`。
  - `serialize_roundtrip_group_priority`:`ServiceConfig` 序列化→反序列化字段保留(runtime overlay `persist_runtime` 直接序列化 `ServiceConfig`,此项是 CRUD 不丢字段的行为保障)。
- [x] **2. Green**:`model.rs` `ServiceConfig` 加两字段(`#[serde(default)]`,`priority: u32` 默认 0,`group: Option<String>`);`config.rs` 无需结构改动(serde 自动),补必要校验注释。**验证**:`cargo test config`。
- [x] **3. Red:排序单测**(`src/supervisor/mod.rs` 内联 `#[cfg(test)]`)
  - `start_order_sorts_by_priority_then_name`:构造 Supervisor 注册 a/b/c(priority 20/10/10),断言 `ordered_names(false) == ["b","c","a"]`(同 10 按 name)。
  - `stop_order_is_reverse`:同上断言 `ordered_names(true) == ["a","c","b"]`。
  - 注意:构造 `Supervisor::new(PathBuf::from(""))` + `add(config)` 即可,不启动进程,纯内存。
- [x] **4. Green**:实现 `ordered_names`,接入四个入口(`start_all`/`stop_all`/`start_auto`/`start_desired`)。**验证**:`cargo test supervisor`。
- [x] **5. Red:启动顺序 e2e**(新文件 `tests/group_priority_e2e.rs` + 新 helper `tests/helpers/stamp_target.rs`)
  - helper 行为(模式参考 `graceful_target.rs`):`stamp_target <name> <stamp_file>` —— 启动即向 `stamp_file` **追加**一行 `start:<name>`;收到停止信号(Windows CTRL_BREAK via `SetConsoleCtrlHandler`,Unix SIGTERM via `tokio::signal::unix::SignalKind::terminate()`)追加 `stop:<name>` 后 exit 0。
  - 测试 `start_all_respects_priority_order`:注册 3 个 stamp_target(priority 10/0/5,其中两个同 priority 验证 name 序),`start_all()` → 全部 Running → 读 stamp 文件断言行序 == 期望启动序。**意图:依赖方按优先级先就绪**。
  - 测试 `stop_all_stops_in_reverse_order`:先 `start_all` 全 Running,再 `stop_all()` → stamp 文件中 `stop:` 行序 == 启动序的逆序。**意图:被依赖方最后停**。
  - 测试 `start_all_continues_after_failure`:priority 最高者用 `common::quick_fail()`(速退 → Failed),断言低优先级 stamp_target 仍被启动。**意图:单个服务故障不阻塞整组拉起**(验证 D4 兜底)。
- [x] **6. Green**:实现就绪推进(`start_all` 内等待目标状态,15s 常量);通过后跑全量 `cargo test`。
- [x] **7. 状态与 API 透出**:`ServiceStatus` 加 `group: Option<String>`、`priority: u32`(从 config 读,序列化自动);CRUD 路由复用 `config::validate` 自动获得新字段——**Red 先行**:`tests/crud_desired_health_e2e.rs` 加断言:create 带 group/priority → GET 回读字段一致 → 重启(重建 Supervisor)后 overlay 恢复不丢。Green 改 `snapshot_status`。
- [x] **8. UI/TUI/示例**:`web/index.html` 表单加 group/priority 输入 + 列表徽章(视觉待用户浏览器确认,仓库惯例);TUI 详情面板加一行;`config/services.example.toml` 三件套加示例(gateway 依赖 rs-iot:rs-iot priority 小、gateway 大)。**验证**:三件 + clippy/fmt 全绿;example 解析测试(read_example)若断言字段则同步。

### 涉及文件

| 文件 | 改动 |
|---|---|
| `src/model.rs` | `ServiceConfig` +2 字段 |
| `src/supervisor/mod.rs` | `ordered_names` + 四入口接入 + 就绪推进 + `ServiceStatus` +2 字段 |
| `src/config.rs` | 单测(+必要时注释) |
| `tests/helpers/stamp_target.rs` | 新增 bin(Cargo.toml 已有 helper bin 模式,照加 `[[bin]]`) |
| `tests/group_priority_e2e.rs` | 新增 e2e |
| `tests/crud_desired_health_e2e.rs` | +CRUD round-trip 断言 |
| `web/index.html` / `src/tui/ui.rs` / `config/services.example.toml` | 展示与示例 |

---

## 需求 2:子进程监听端口发现

### 目标

- 对每个 Running 服务,周期发现其(含子孙进程)实际监听的 TCP 端口与绑定的 UDP 端口,进入 `ServiceStatus` 供 API/UI 展示。

### 范围外

- **UDP 健康检查**:UDP 无 connect 探活等价物,需协议感知,明确排除。
- 端口→健康检查自动关联(`health.port = "auto"`):发现落地后的后续增强,本文仅预留方向。
- 连接级信息(ESTABLISHED 等):只关心 LISTEN/绑定,不采集连接表。

### 设计

**分层:平台采集 → 中性行 → 纯过滤(可测核心)**,新模块 `src/supervisor/ports.rs`(与 health/metrics 平级):

```rust
/// 中性 socket 行(平台采集层的输出,过滤层的输入)。
pub(crate) struct RawSocketRow {
    pub proto: Proto,            // Tcp | Udp
    pub local_addr: IpAddr,
    pub local_port: u16,
    pub tcp_state: Option<TcpState>, // Windows MIB/Linux ss 状态;UDP 恒 None
    pub pids: Vec<u32>,          // 该 socket 关联的 PID(平台表给出)
}

/// 纯过滤核心:从全表 socket 行中筛出 pid_set(服务 PID 子树)监听的端口。
pub(crate) fn filter_listening(rows: &[RawSocketRow], pid_set: &HashSet<u32>) -> Vec<ListeningSocket>;

/// 对外暴露(ServiceStatus 字段)。
#[derive(Serialize, Clone)]
pub struct ListeningSocket {
    pub proto: &'static str,     // "tcp" | "udp"
    pub local_addr: IpAddr,      // 含绑定范围(0.0.0.0 vs 127.0.0.1),边缘安全审计有用
    pub local_port: u16,
}
```

**平台采集(决策点 P1 两选一,默认 netstat2)**:

- Windows:`GetExtendedTcpTable`/`GetExtendedUdpTable`;Linux:`/proc/net/tcp{,6}`、`udp{,6}` inode→PID。手写约 250 行平台代码,warden 侧还需自担两平台解析正确性(Linux 路径在 Windows 开发机上无法测)。
- [netstat2](https://crates.io/crates/netstat2)(0.11.2,2025-08 更新,~90 万下载):底层 OS API(非 shell netstat),Win/Linux/macOS,一个依赖屏蔽全部平台差异,`associated_pids`/TCP state 已解析。同步 API,包 `tokio::task::spawn_blocking` 调用。
- 选 netstat2 的理由:warden 目标平台仅 Win/Linux,自写平台代码的收益(去一个依赖)小于风险(解析 bug 自担);netstat2 不合适时替换成本 = 重写采集层一个函数,`RawSocketRow`/过滤层不动。

**PID 子树匹配(必须,不是优化)**:被监护服务常是启动器形态(如 `reasonix serve` 拉工作进程),真正监听的可能是孙子进程,精确 PID 过滤会漏。子树构建复用 metrics task 已有的 sysinfo `System`:刷新时需含**全量进程 + parent 关系**(现状 `metrics::refresh` 只按服务 PID 定向刷,需扩展为全表刷新;边缘设备 2s 一次全表,sysinfo 可接受)。BFS 收集子树,visited 防环,PPID 缺失即终止。已知局限:采样间隔内 PID 复用理论上可致误报,2s 窗口概率可忽略,文档记录不处理。

**集成点**:`spawn_metrics` 周期(2s)内顺带刷新端口(端口表与 metrics 同源同频,不另起 task):

```
每轮:refresh(sysinfo 全表) → 对每个 Running 服务:算 PID 子树 →
spawn_blocking(netstat2 全表) 一次(共享结果)→ filter_listening → 写 ProcInner.ports
```

`ProcInner` + `ports: Vec<ListeningSocket>`;`snapshot_status` 透出 `listening_ports`。

### 决策点(待确认)

| # | 问题 | 方案默认 | 备选 |
|---|---|---|---|
| P1 | 采集实现 | netstat2 依赖 | windows-sys IpHelper + /proc 手写(去依赖,自担解析) |
| P2 | 刷新频率 | 跟随 metrics 2s 周期 | 独立 5s task(收益不明,弃) |
| P3 | 孙子进程 | PID 子树匹配(必做) | 仅精确 PID(漏启动器形态服务,不满足需求,弃) |
| P4 | sysinfo 刷新扩展 | 全表 + parent(metrics task 内,一次 refresh 两用) | 增量定向刷(建树需要全表,做不到,弃) |

### TDD 实施步骤

- [x] **1. Red:过滤核心单测**(`src/supervisor/ports.rs` 内联 `#[cfg(test)]`,纯数据构造,跨平台)
  - `keeps_tcp_listen_for_exact_pid` / `excludes_non_listen_tcp`(SYN_SENT/ESTABLISHED 行被滤掉)。
  - `includes_grandchild_pid`(pid_set 含多个 PID 时命中孙进程行)——**意图:启动器形态服务可见真实监听者**。
  - `udp_has_no_state_all_bound_kept`(UDP 行无状态全保留)。
  - `ignores_unrelated_pids` / `keeps_ipv6_rows`。
- [x] **2. Green**:实现 `RawSocketRow`/`Proto`/`TcpState`/`filter_listening`/`ListeningSocket`。**验证**:`cargo test ports`。
- [x] **3. Red:平台采集 e2e**(新 helper `tests/helpers/port_listener_target.rs` + 测试并入新文件 `tests/ports_e2e.rs`)
  - helper 行为:绑定 `127.0.0.1:0` TCP listener + UDP socket(**随机端口,遵守"勿固定端口"铁律**),向 stdout 打印自报行 `PORT tcp 127.0.0.1 <port>` / `PORT udp 127.0.0.1 <port>`,然后长跑等信号退出(Unix 收 SIGTERM;参考 stamp/graceful helper 模式);`--grandchild` 参数再 spawn 一个自身实例(孙进程 stdout 继承 → warden 同一管道可读到孙的自报行)。
  - 测试 `discovers_listener_ports`:注册 1 个服务 → start → 从 `log_hub(name).snapshot()` 提取 `PORT` 自报行得**期望端口真值** → 轮询 `status(name).listening_ports` 直到包含全部期望项。**意图:两个独立信源(子进程自报 vs OS 端口表)一致,即"warden 看见了服务真实监听的端口"**。
  - 测试 `discovers_grandchild_ports`:`--grandchild` 场景,断言孙进程自报的端口同样出现在 `listening_ports`。
- [x] **4. Green**:接入 netstat2(`Cargo.toml` +1 依赖)实现采集转换;扩展 `metrics::refresh` 全表;`spawn_metrics` 周期集成;`ProcInner.ports` + `snapshot_status` 透出。**验证**:`cargo test`全量(Windows 本机跑通;Linux 路径走 netstat2 自身 CI,warden 侧 e2e 在 Linux runner 可用时补跑,否则标注)。
- [x] **5. API/UI**:`GET /services` 自然带出(序列化自动);`web/index.html` 服务详情展示端口徽章(点击 `127.0.0.1:port` 可拼 ui_url 逻辑不做,仅展示);TUI 详情面板加端口行。**验证**:三绿 + UI 浏览器人工确认(仓库惯例)。

### 涉及文件

| 文件 | 改动 |
|---|---|
| `src/supervisor/ports.rs` | 新增:类型 + 过滤核心 + 平台采集转换 |
| `src/supervisor/metrics.rs` | refresh 扩展为全表 + parent |
| `src/supervisor/mod.rs` | `ProcInner.ports` + `spawn_metrics` 集成 + `ServiceStatus.listening_ports` |
| `Cargo.toml` | +netstat2(+helper bin) |
| `tests/helpers/port_listener_target.rs` | 新增 bin |
| `tests/ports_e2e.rs` | 新增 e2e |
| `web/index.html` / `src/tui/ui.rs` | 展示 |

---

## 实施顺序与依赖

```
需求1 步骤1-6(排序+顺序启停,独立可交付)
  → 需求1 步骤7-8(透出,收尾)
需求2 步骤1-2(过滤核心,与需求1无依赖,可并行)
  → 需求2 步骤3-5(采集+集成)
〔自升级:待 RESEARCH 文档 §5 取舍定论后另立计划,复用需求1 的 ordered_names〕
```

预计测试增量:需求 1 约 +8(单测 5 + e2e 3),需求 2 约 +7(单测 5 + e2e 2)。

## 风险与开放问题

- **start_all 就绪推进的 15s 上限**:慢启动服务(如 rs-iot 首启加载 lux)可能超时被"跳过"——但跳过仅指不再等待,不取消启动,无功能损害;若现场普遍慢启动,再提为配置项(YAGNI,先不做)。
- **stamp_target 的 Unix SIGTERM**:现有 `graceful_target` Unix 侧监听 SIGINT(ctrl_c),而 warden stop 发 SIGTERM;stamp_target 直接监听 SIGTERM,与生产信号路径一致(顺带修了测试 helper 与生产信号不一致的隐患)。
- **netstat2 全表采集开销**:每 2s 一次 GetExtendedTcpTable/proc 解析,进程数多的机器有毫秒级成本,spawn_blocking 不阻塞 runtime;现场若见异常再评估 P2 降频。
- **Windows 测试环境**:需求 2 的 Linux 采集路径在 Windows 开发机不可测,依赖 netstat2 自身 CI 覆盖;warden Linux e2e 待有 Linux runner 补(与 ROADMAP 既有"systemd 未实测"同批)。

## 实施记录(2026-08-15 完成)

决策点 D1–D5 / P1–P4 均按「方案默认」落地(用户已确认)。最终 **69 测试全绿**(实施前 46),`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` clean。实际增量:需求 1 单测 5 + e2e 4 + CRUD round-trip 1;需求 2 单测 10(过滤 7 + 子树 3)+ e2e 2。

与计划的偏差/补充:

1. **P4 无需改动**:`metrics::refresh` 现状已是 `refresh_all()`(全表含 parent),`children_index` 直接消费,无需"扩展为全表刷新"。
2. **就绪推进统一走 `start_ordered` 谓词**:`start_all`/`start_auto`/`start_desired` 三个入口收敛到一个实现(计划中 start_desired 未明确,实施时统一,恢复语义与手动 start-all 一致)。
3. **clippy `zombie_processes`** 拦截 `port_listener_target` 的孙进程 spawn(有意不 wait,由 Job Object/进程组回收):按项目约定 `#[expect(clippy::zombie_processes)]` + 注释。
4. **`tests/common::long_runner` 补 `#[allow(dead_code)]`**:两个新 e2e 文件不用它,触发 dead_code 警告(clippy -D 挂);与既有 `quick_fail` 同款注释。
5. **helper 自报行走 stderr**(`eprintln!` 无缓冲):stdout 在管道下是块缓冲,行可能延迟 flush 导致 e2e 读不到(实施时预判规避)。
6. **端口采集失败降级**:netstat2 采集失败仅 `warn` + 本轮空表(端口列空,不影响 metrics 采样),不中断 metrics task。
7. **netstat2 依赖面**:Windows 零额外传递依赖(自写 FFI);Linux/macOS 构建才拉 netlink/bindgen 系(warden Linux 交叉构建需 libclang,与上文 Linux 风险项同批)。

新文件:`src/supervisor/ports.rs`、`tests/helpers/stamp_target.rs`、`tests/helpers/port_listener_target.rs`、`tests/group_priority_e2e.rs`、`tests/ports_e2e.rs`。
