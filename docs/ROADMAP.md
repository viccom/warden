# warden 路线图与进度

> **跨会话接续入口**:新会话先读本文件的「当前进度」,再按需查 [`DESIGN.md`](./DESIGN.md) 对应章节,然后从下一个 `[ ]` 步骤继续。每完成一步把 `[ ]` 改 `[x]` 并更新「最后更新」日期,必要时写「变更日志」。

- **最后更新**:2026-08-14
- **当前阶段**:Phase 1 地基(✅ 完成:31 测试绿 + clippy clean + 冒烟通过)
- **下一步**:Phase 2(daemon 自注册 OS 服务,后台常驻 + 开机自启)

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
- [x] **daemon 作为 Windows Service ✅**:service_main(SCM Stop → mpsc → StopPending 30s → shutdown.cancel → worker 线程 run_app_with_shutdown → Stopped → exit 0)。`run`(前台 ctrl_c)与 Service(SCM Stop)共享 `run_app_with_shutdown`。
- [x] **会话 0 console graceful(AllocConsole)✅**:Service 模式(会话 0 默认无 console)启动时 `AllocConsole` 创建不可见 console → 子进程继承 → CTRL_BREAK 链路保持。**实测 `sc stop warden` → rs-iot `lux SAVE ok`**(会话 0 graceful 达成,Phase 1 优雅停止在 Service 模式仍有效)。
- [ ] UAC 自提权(`ShellExecuteW "runas"`)——当前 install 提示需管理员手动运行
- [ ] CREATE_NO_WINDOW(子进程在服务会话不弹窗——rs-iot/reasonix 控制台程序)
- [ ] GBK/CP936 → UTF-8 解码被监护进程输出
- [ ] Linux systemd unit + systemctl enable(框架已写 `systemd.rs`,未实测)
- [ ] SCM/Win32 错误码翻译 + 1066 退出码治理(照 rs-iot,数据优先;SAVE 已执行)

---

## Phase 3 —— ratatui TUI 客户端

> 连本地或远程 HTTP API 的终端客户端。设计待细化。

- [ ] `warden tui` 子命令,reqwest 连 API
- [ ] 服务表格(名称 / 状态 / PID / CPU% / 内存 / 重启次数)+ 虚拟滚动
- [ ] 选中服务详情面板(配置、最近退出、健康状态)
- [ ] 实时日志面板(SSE 订阅 + 滚动 + 暂停 + 多服务切换)
- [ ] 快捷键:start/stop/restart(s/x/r)、日志切换(l)、全选操作(a/z)、过滤(/)、退出(q)
- [ ] 连接状态指示 + 重连
- [ ] 暗色主题(参考 serviceMgr-tui styles.go)

---

## Phase 4 —— 增强

- [ ] TCP/HTTP 健康检查完整实现 + 告警(webhook / 日志)
- [x] **优雅停止 ✅(2026-08-14 完成)**:`stop` 改为发信号 → `graceful_timeout` → 超时强杀。Windows 发 `CTRL_BREAK_EVENT`(独立 process group 精确投递;CTRL_C 不跨 group 是 Windows quirk)。**配套 rs-iot 已加 CTRL_BREAK 监听**(src/lib.rs `shutdown_signal`),实测 `warden stop rs-iot` → 日志 `lux SAVE ok` → 数据安全达成。Linux 走 SIGTERM。
- [x] **杀进程树 ✅(2026-08-14 完成)**:每个子进程一个 Job Object(`KILL_ON_JOB_CLOSE`),stop 时 `TerminateJobObject` 杀整棵树 + warden 崩溃时子进程树全死(无孤儿)。helper e2e 验证(stubborn + child → force_kill 杀树)。
- [ ] 运行时配置 CRUD(POST/PUT/DELETE `/services`,免改文件)
- [ ] Web 前端(复用已就绪 API,rust-embed 嵌入,参考 rs-iot gateway)
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
