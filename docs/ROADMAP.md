# warden 路线图与进度

> **跨会话接续入口**:新会话先读本文件的「当前进度」,再按需查 [`DESIGN.md`](./DESIGN.md) 对应章节,然后从下一个 `[ ]` 步骤继续。每完成一步把 `[ ]` 改 `[x]` 并更新「最后更新」日期,必要时写「变更日志」。

- **最后更新**:2026-08-14
- **当前阶段**:Phase 2 收尾 + Web UI 雏形(Windows Service + 会话 0 graceful + GBK 解码 + 浏览器界面;36 测试绿 + clippy/fmt clean)
- **下一步**:Phase 2 剩余(UAC 自提权 / CREATE_NO_WINDOW / systemd 实测 / SCM 错误码)或 Phase 4 Web 完善

---

## Phase 1 —— 地基(config + 监护引擎 + 日志 + HTTP API + 测试)✅

> 设计细节见 [DESIGN.md §5–§11](./DESIGN.md)。

- [x] **0. 建仓库 + 文档**:git init + Cargo.toml/工具链 + docs/DESIGN.md + docs/ROADMAP.md + services.example.toml
- [x] **1. 脚手架**:src 骨架(main.rs clap + lib.rs)+ cargo build 验证依赖
- [x] **2. error + model + config**:WardenError(+IntoResponse)/ ServiceConfig+ProcState+RestartPolicy+HealthCheck / toml 解析+Default+路径查找+校验(坏项跳过)
- [x] **3. model**:并入第 2 步
- [x] **4. logs**:LogHub(VecDeque 环缓冲 + broadcast + 按日轮转文件)
- [x] **5. supervisor**:引擎 + 状态机 + backoff + start/stop/restart/start-all/stop-all + supervisor_e2e
- [x] **6. metrics**:sysinfo 周期采样(验证 Running 进程 metrics 非 0)
- [x] **7. api**:build_router + token 鉴权中间件 + SSE 日志流 + api_flow
- [x] **8. main + lib**:run_app(clap run 前台 + tracing 多 layer + graceful shutdown + auto_start)+ 冒烟全链路验证
- [x] **9. 示例配置**:services.example.toml + read_example 解析测试
- [x] **10. 收尾**:31 测试全绿 + `clippy --all-targets -- -D warnings` clean

### Phase 1 验收(已达成 ✅)
- ✅ `warden run --config <path>` 拉起配置中的进程(冒烟:ping 服务 start→running→logs→stop 全链路)
- ✅ HTTP API 全端点可用:services 列表/详情、start/stop/restart、logs 快照 + SSE、metrics、health、reload、start-all/stop-all
- ✅ 崩溃自动重启(auto_restart)按退避策略工作,超 max_retries 进 Failed(e2e 验证)
- ✅ token 鉴权 + `/health` 白名单(api_flow 验证)
- ✅ `cargo test`(31)+ `clippy -D warnings` 干净

### 测试统计(31)
| 类别 | 数量 | 内容 |
|---|---|---|
| 单测 | 16 | config 解析/校验、model backoff/状态、logs push/snapshot/broadcast/轮转 |
| api_flow | 7 | health/list/start-stop/logs/404/鉴权拒绝/鉴权放行+白名单 |
| supervisor_e2e | 7 | start-stop/速退 Failed/重启熔断/404/list/logs 捕获/metrics 采样 |
| read_example | 1 | services.example.toml 含 rs-iot 三件套 |

---

## Phase 2 —— daemon 自注册 OS 服务(后台常驻 + 开机自启)

> 参考 rs-iot `src/service/`(windows-service crate 范式)。核心已完成 ✅。

- [x] **install / uninstall / service 子命令 ✅(2026-08-14)**:`sc.exe create/delete`(binPath=`<warden> service` start=auto)+ `define_windows_service!` + `service_dispatcher` + `service_control_handler`。实测 install/uninstall 通过。
- [x] **Service 模式实测全链路 ✅(2026-08-14 用户实测)**:管理员下 `install → sc start(RUNNING)→ curl health(running=1)→ 浏览器 UI 可见日志/启停 → sc stop(STOPPED,graceful,gbk_target 无残留)→ uninstall`。配置须绝对路径(exe_dir/config/services.toml,会话 0 cwd=System32)。**遗留:stop 后 `sc query` 的 WIN32_EXIT_CODE 显示 1066**(rs-iot 同结构同现象,服务实际正常停止/退出码 0,属 SCM 显示层,列入下方 1066 治理待办)。
- [x] **daemon 作为 Windows Service ✅**:service_main(SCM Stop → mpsc → StopPending 30s → shutdown.cancel → worker 线程 run_app_with_shutdown → Stopped → exit 0)。`run`(前台 ctrl_c)与 Service(SCM Stop)共享 `run_app_with_shutdown`。
- [x] **会话 0 console graceful(AllocConsole)✅**:Service 模式(会话 0 默认无 console)启动时 `AllocConsole` 创建不可见 console → 子进程继承 → CTRL_BREAK 链路保持。**实测 `sc stop warden` → rs-iot `lux SAVE ok`**(会话 0 graceful 达成,Phase 1 优雅停止在 Service 模式仍有效)。
- [x] **UAC 自提权 ✅(2026-08-14,实测验证通过)**:`is_elevated()`(OpenProcessToken+GetTokenInformation TokenElevation)检测非管理员 → `relaunch_elevated`(ShellExecuteW "runas")以管理员重跑自己,新进程执行 install/uninstall。install/uninstall 均提权。windows-sys 需 `Win32_UI_Shell` + `Win32_UI_WindowsAndMessaging`(ShellExecuteW 的 cfg 门控,踩坑记录)。**用户实测:非管理员 install 自动提权,服务成功安装**(用户系统 UAC 自动确认,无弹窗,链路正常)。
- [x] **CREATE_NO_WINDOW(2026-08-14 分析结论:不做)**:Service 模式跑在**会话 0**(Windows Vista+ 会话隔离),服务及其子进程的窗口/console 用户桌面不可见——"弹窗"问题在 Service 模式下不成立(仅已废弃的"允许与桌面交互"配置例外)。且 CREATE_NO_WINDOW 会移除子进程的 console 成员身份,`GenerateConsoleCtrlEvent` 投递失效 → **破坏 CTRL_BREAK graceful(rs-iot lux SAVE 数据安全)**,后果不可接受。结论:不做,标记关闭。
- [x] **GBK/CP936 → UTF-8 解码 ✅(2026-08-14)**:`pipe_reader` 改 `read_until(b'\n')` 读原始字节行 + `encoding_rs` 按编码解码;per-service `output_encoding`(None=UTF-8,支持 `gbk`/`cp936` 别名,未知回退 UTF-8 + warn)。修复原 `BufReader::lines()` 对 GBK 整行静默丢失(遇非法字节报 IO Error 被吞)。GBK 双字节尾字节不含 0x0A,按行切分安全;UTF-16 不支持(行内含 0x0A)。+2 tokio 测试(34→36),clippy clean。**未对真实 rs-iot GBK 输出实测**(单测用已知 GBK 字节覆盖核心逻辑,真实编码需现场确认后配置)。
- [ ] Linux systemd unit + systemctl enable(框架已写 `systemd.rs`,未实测)
- [x] **1066 退出码治理 ✅(2026-08-14,验证通过)**:根因:windows-service `ServiceExitCode::ServiceSpecific(n)` 把 `dwWin32ExitCode` 置为 `ERROR_SERVICE_SPECIFIC_ERROR`(1066)——此前所有状态报告都用 `ServiceSpecific(0)`,SCM 恒显 1066。改 `ServiceExitCode::NO_ERROR`(=Win32(NO_ERROR));配置加载失败仍 `ServiceSpecific(1)`。**用户管理员实测:install→start→stop→query,WIN32_EXIT_CODE 由 1066 → 0 (0x0)**。SCM/Win32 错误码翻译另列。

---

## Phase 3 —— ratatui TUI 客户端

> 连本地或远程 HTTP API 的终端客户端。设计待细化。

- [x] **`warden tui` 子命令 ✅(2026-08-14)**:ratatui 0.30 + crossterm + reqwest + reqwest-eventsource(SSE 自动重连)。`--url`(默认 127.0.0.1:8789)/`--token`。三模块 `src/tui/{api,mod,ui}`:ApiClient(轻量反序列化,state 用 Value 解析)+ 事件循环(1s 状态刷新 + SSE 日志流 mpsc)+ 渲染(顶栏连接状态/服务表格/详情|日志面板/帮助栏)。快捷键 s/x/r/a/z/l/p/c//q。**验证**:tui_api_e2e 契约测试(真 HTTP:list/启停/start-all/stop-all/logs/SSE)+ 真实运行冒烟(连接 8791 显示 running=2 全表渲染)。38 测试全绿 + clippy/fmt clean。
- [ ] 服务详情面板(配置、最近退出、健康状态)— 详情面板已有基础字段,健康状态等增强待做
- [ ] 连接状态指示 + 重连 — 已有(顶栏 connected/reconnecting),断线重连由 eventsource 保证
- [ ] 暗色主题(参考 serviceMgr-tui styles.go)— 已用暗色,样式微调待做

---

## Phase 4 —— 增强

- [ ] TCP/HTTP 健康检查完整实现 + 告警(webhook / 日志)
- [x] **优雅停止 ✅(2026-08-14 完成)**:`stop` 改为发信号 → `graceful_timeout` → 超时强杀。Windows 发 `CTRL_BREAK_EVENT`(独立 process group 精确投递;CTRL_C 不跨 group 是 Windows quirk)。**配套 rs-iot 已加 CTRL_BREAK 监听**(src/lib.rs `shutdown_signal`),实测 `warden stop rs-iot` → 日志 `lux SAVE ok` → 数据安全达成。Linux 走 SIGTERM。
- [x] **真实三件套优雅停止全套实测 ✅(2026-08-14 用户复测)**:example.toml(8789,auth_token 生效,API 需 `Authorization: Bearer`)三件套全拉起(rs-iot :8790 / reasonix :8787 / gateway :8080 全通)。`stop rs-iot` → **2.2s graceful,日志 `lux SAVE ok (snapshot flushed)` → rs-iot stopped** 数据安全 ✅;`stop reasonix` → 0.1s(Go 默认响应 CTRL_BREAK 即退)、:8787 关闭 ✅;`stop rsiot-gateway` → 0.06s、:8080 关闭 ✅;start 恢复 rs-iot 新 PID ✅;收尾零残留(仅用户现场自开的 reasonix 保留)。
- [x] **杀进程树 ✅(2026-08-14 完成)**:每个子进程一个 Job Object(`KILL_ON_JOB_CLOSE`),stop 时 `TerminateJobObject` 杀整棵树 + warden 崩溃时子进程树全死(无孤儿)。helper e2e 验证(stubborn + child → force_kill 杀树)。
- [ ] 运行时配置 CRUD(POST/PUT/DELETE `/services`,免改文件)
- [x] **Web 前端最小版 ✅(2026-08-14,雏形)**:`GET /` 返回嵌入的单页面(`include_str! web/index.html`,零新依赖),暗色双栏:服务列表 + SSE 实时日志 + start/stop + 可选 token(配 token 时 `EventSource` 无法带 header → 降级 1.5s 轮询)。新增 `routes_ui.rs` + auth 白名单加 `/`。**完整多资源版待做**(rust-embed 嵌入、metrics 图表、运行时 CRUD、JWT 登录)。
- [ ] desired-state 持久化(daemon 重启后恢复期望状态)
- [ ] 鉴权升级 JWT + login(加 Web 时)

---

## 变更日志

- **2026-08-14**:仓库初始化。完成 Phase 1 第 0 步(脚手架 + DESIGN.md + ROADMAP.md + services.example.toml)。技术栈对齐 rs-iot,架构定为自带监护 + daemon 自注册 OS 服务(P2)。
- **2026-08-14(续)**:完成 Phase 1 第 1-5 步(error / model / config / logs / supervisor + 22 测试全绿,e2e 0.68s)。发现并记录局限:`stop` 不杀进程树(cmd 包装的孙子孤儿),Phase 4 用 Job Object 解决。
- **2026-08-14(完)**:**Phase 1 全部完成**。第 6-10 步(metrics / api+鉴权+SSE / run_app+tracing+graceful / example 验证 / clippy clean)。累计 31 测试全绿,冒烟测试验证 warden run 全链路可用。下一步 Phase 2。
- **2026-08-14(真实验证)**:用真实 rs-iot 三件套验证 Phase 1。✅ 三件套全部拉起/监护/日志捕获/metrics/直接 exe 干净 stop 全工作。⚠️ 实测确认两个 Phase 4 关键项并**调整优先级**:① reasonix(cmd→node)stop 后 node 孤儿(:8787 仍 200);② **stop 强杀使 rs-iot 跳过 lux SAVE(数据风险)→ 优雅停止对 rs-iot 是数据安全关键,优先级提升到 Phase 2 之前考虑**。
- **2026-08-14(reasonix Go 二进制)**:reasonix 改用官方 Go 单二进制(`E:\rsiot-field\bin\reasonix.exe` v1.25.1),`services.example.toml` 去掉 `cmd /c` 包装。实测 stop reasonix 后 :8787 立即 000、无 node/reasonix 残留——**孤儿问题在配置层面解决**(不依赖 Phase 4 Job Object)。CLI 用单横杠参数 `-addr`/`-auth`/`-token`。
- **2026-08-14(优雅停止 + 杀进程树 ✅)**:原 Phase 4 两项提前完成。warden:`CTRL_BREAK` 信号(独立 group 精确投递)+ `graceful_timeout` + Job Object(`KILL_ON_JOB_CLOSE` 杀树/崩溃保护),34 测试 + clippy clean,helper e2e 验证 graceful/强杀/杀树。**发现并记录 Windows 限制**:`CTRL_C_EVENT` 对独立 process group 不投递(quirk),只有 `CTRL_BREAK_EVENT` 跨 group;tokio `ctrl_c()` 不响应 CTRL_BREAK。**配套 rs-iot 改动**:src/lib.rs 加 `shutdown_signal`(SetConsoleCtrlHandler 监听 CTRL_C+CTRL_BREAK),实测 `warden stop rs-iot` → `lux SAVE ok`(数据安全达成)。
- **2026-08-14(Phase 2 核心 ✅)**:warden 自注册 Windows Service。install/uninstall(sc.exe)+ service 子命令(define_windows_service + service_dispatcher + service_control_handler)+ service_main(SCM Stop → graceful)+ `run_app_with_shutdown` 共享(前台/Service)。**核心挑战解决**:Service 模式(会话 0)AllocConsole → 子进程继承 console → CTRL_BREAK 仍触发 rs-iot SAVE(实测 `sc stop warden` → `lux SAVE ok`)。clippy clean。剩余:UAC 自提权、CREATE_NO_WINDOW、GBK、systemd 实测、1066 治理。
- **2026-08-14(GBK 输出解码 ✅)**:Phase 2 收尾项之一。`supervisor/proc.rs` `pipe_reader` 由 `BufReader::lines()`(强制 UTF-8,遇非法字节报 IO Error 被 `while let Ok(...)` 吞掉 → GBK 整行静默丢失)改为 `read_until(b'\n')` 读原始字节行 + `encoding_rs::Encoding::decode` 按配置解码。新增 per-service `output_encoding: Option<String>`(model.rs),None=UTF-8;`resolve_encoding` 用 `for_label` 解析(支持 `gbk`/`gb2312`/`cp936`/`utf-8` 别名,未识别回退 UTF-8 + warn)。GBK 双字节尾字节(0x40–0x7E / 0x80–0xFE)不含 0x0A,按行切分安全;UTF-16 不支持(行内字节含 0x0A,doc 已注明)。非法字节以 U+FFFD 替换(较丢整行更友好)。+2 tokio 测试(GBK 解码端到端 + UTF-8 回归),测试 34→36 全绿,clippy clean。**遗留发现**:`cargo fmt --all --check` 仓库当前不干净,且系**预先存在**(涉及 `api/`、`main.rs`、`signal.rs` 等多个本次未触碰文件),非本次引入(已于本次会话用 `cargo fmt --all` 全项目格式化修复)。
- **2026-08-14(Web UI 雏形 ✅)**:Phase 4 最小版前置。`GET /` 返回嵌入单页面(`include_str! web/index.html`,零新依赖),暗色双栏:服务列表(2s 刷新状态 + 行内 start/stop)+ 实时日志面板(SSE 订阅 `/logs/stream`,暂停/清空/自动滚动,stdout/stderr 分色)+ 可选 token(localStorage;配 token 时 `EventSource` 不能带 header → 降级 1.5s 轮询)。新增 `src/api/routes_ui.rs`(handler 返回 `Html`)+ auth 白名单加 `/`。日志用 `textContent` 渲染防 XSS。**自验证**:curl `/` 返回 `text/html`(浏览器渲染非纯文本);修复 JS 一处 bug(services 返回 `{services:[...]}` 非裸数组)。fmt + clippy + test(36)全绿。UI 视觉/交互待用户浏览器最终确认。
- **2026-08-14(Ctrl-C 优雅退出修复 + 管理/监视全链路实测)**:用户实测发现前台 Ctrl-C 后 warden 以 0xC000013A 退出(未走 graceful)。根因两层:① tokio `ctrl_c()` 接收端在首次事件后 drop,再次 Ctrl-C 时 tokio handler 返回 FALSE → std 默认 handler `ExitProcess(0xC000013A)` 强杀;② SSE 长连接(浏览器 UI 挂着时)使 axum graceful shutdown 无限等待 → 进程卡住,用户再按 Ctrl-C 触发①。**修复**:`supervisor/signal.rs` 加 `install_console_shutdown`(Windows 自有 `SetConsoleCtrlHandler` 永久拦截 CTRL_C/CTRL_BREAK → 触发 shutdown;多次 Ctrl-C 都走 graceful);`lib.rs` serve 设 5s 强断上限(**从 shutdown 后起算**,曾误写成启动起算导致 daemon 5s 必退,已修正 + e2e 加"8s 不自杀"断言防护)。**测试**:+`tests/shutdown_e2e.rs`(SSE 挂着时 cancel 应限期退出 + 不自杀,绿);`tests/shutdown_console_e2e.rs`(真实 console 注入 CTRL_C 验退出码 0,**已 #[ignore] 待后期**:CREATE_NEW_CONSOLE 形态下 AttachConsole+GenerateConsoleCtrlEvent 对 warden 不可达,属测试环境 console 事件分发怪癖,修复本身已由 shutdown_e2e 覆盖)。37 测试绿 + clippy/fmt clean。**管理/监视全链路实测通过**:restart(PID 18164→5880)/ stop(→stopped)/ start(→running,新 PID)✅;监视 services 列表(状态/PID/CPU/内存/重启次数)+ logs 快照(GBK 中文正确)+ metrics 端点 ✅。Windows 日志显示类问题暂略,待后期。
