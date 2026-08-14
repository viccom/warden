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

> 参考 serviceMgr-tui 的 OS 服务注册能力,反向用于注册 daemon 自身。设计待细化。

- [ ] `windows-service` crate 调 SCM:`install` / `uninstall` / `service` 子命令
- [ ] daemon 作为 Windows Service 启动(进 SCM 事件循环,on Start 跑监护主逻辑,on Stop 触发 CancellationToken)
- [ ] UAC 提权(`ShellExecuteW "runas"`,检测 Administrators 组)
- [ ] Windows 下被监护控制台进程的 `CREATE_NO_WINDOW` 处理(避免服务会话弹窗/失败)
- [ ] GBK/CP936 → UTF-8 解码被监护进程输出(`encoding_rs`)
- [ ] Linux systemd unit 模板 + `systemctl enable/start`;macOS launchd plist(次要)
- [ ] SCM/Win32 错误码翻译(对齐 serviceMgr-tui `manager_common.go`)

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
- [ ] 优雅停止(Linux SIGTERM→SIGKILL / Windows GenerateConsoleCtrlEvent)
- [ ] **杀进程树(Phase 1 已知局限)**:`stop` 的 TerminateProcess 只杀直接子进程;被监护程序若 spawn 子进程(如 `cmd /c reasonix → node`),孙子会残留孤儿(e2e 已验证:孤儿跑满 ping 时长)。需用 Windows Job Object(KILL_ON_JOB_CLOSE)杀整棵进程树
- [ ] 运行时配置 CRUD(POST/PUT/DELETE `/services`,免改文件)
- [ ] Web 前端(复用已就绪 API,rust-embed 嵌入,参考 rs-iot gateway)
- [ ] desired-state 持久化(daemon 重启后恢复期望状态)
- [ ] 鉴权升级 JWT + login(加 Web 时)

---

## 变更日志

- **2026-08-14**:仓库初始化。完成 Phase 1 第 0 步(脚手架 + DESIGN.md + ROADMAP.md + services.example.toml)。技术栈对齐 rs-iot,架构定为自带监护 + daemon 自注册 OS 服务(P2)。
- **2026-08-14(续)**:完成 Phase 1 第 1-5 步(error / model / config / logs / supervisor + 22 测试全绿,e2e 0.68s)。发现并记录局限:`stop` 不杀进程树(cmd 包装的孙子孤儿),Phase 4 用 Job Object 解决。
- **2026-08-14(完)**:**Phase 1 全部完成**。第 6-10 步(metrics / api+鉴权+SSE / run_app+tracing+graceful / example 验证 / clippy clean)。累计 31 测试全绿,冒烟测试验证 warden run 全链路可用。下一步 Phase 2。
