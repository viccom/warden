# 实施方案:warden 桌面版(Tauri 2)

> **状态**:方案定稿,分期实施。P1 = 本次交付。
> **日期**:2026-08-15
> **需求来源**:用户三条——①覆盖 CLI 版功能但只前台运行、全局单实例、最小化托盘/托盘退出;②默认管理自己子进程 + 可添加其他 warden 节点(本机 CLI / 远程)统一管理;③前端 UI 重写(不复用 CLI web),支持 group 过滤、group 级启停,设计自由度大。

## 1. 选型:Tauri 2(已定)

| 候选 | 结论 | 理由 |
|---|---|---|
| **Tauri 2** ✅ | 采用 | Rust 后端 `path` 依赖直接复用 warden lib(Supervisor/LogHub/health/ports 零重写);系统托盘核心内置;`tauri-plugin-single-instance` 官方插件;前端任意技术栈;体积 ~10MB |
| egui / iced | 排除 | 纯 Rust GUI 写"丰富美观"的多节点管理 UI 成本过高,生态弱 |
| Electron | 排除 | 体积 100MB+,Rust 复用退化为 sidecar,无优势 |

环境已验证:WebView2 Runtime 151 已装(Win10 LTSC)、node 22 / pnpm 8 / rustc 1.97。前端用 **Vue 3 + Vite(纯 JS)**——多节点聚合状态 + 组件复用需要框架,但不引 TS/pinia 等额外摩擦。

## 2. 架构

```
warden/                     # 现有 crate 不动,root Cargo.toml 追加 [workspace]
└─ desktop/
   ├─ src-tauri/            # Rust 桌面端 crate「warden-desktop」
   │   ├─ main.rs/lib.rs    # tauri app:单实例(最先注册)→ 内嵌 daemon → 托盘
   │   ├─ daemon.rs         # 进程内起 axum(复用 build_state/build_router)
   │   └─ nodes.rs          # 节点注册表持久化 + Tauri 命令(list/add/remove)
   └─ src/                  # Vue 3 前端(重写,不复用 web/index.html)
```

**核心原则:本地节点与远程节点统一走 HTTP API。**
- 桌面版启动时进程内起 warden daemon(Supervisor + axum router),绑 `127.0.0.1:0` 随机端口 + 进程内生成的随机 token(防本机其他进程裸连),实际端口经 Tauri 初始化事件传给前端
- 本地节点自动注入节点列表(不可删);远程节点 = 用户添加(url + token + 显示名),持久化在应用数据目录 `nodes.json`
- 前端一套 HTTP/SSE 客户端代码覆盖所有节点;CLI 版 API 全量复用(服务/日志/CRUD/健康/端口/组启停)
- 随机端口避免与用户手动跑的 CLI warden(8789)冲突

**桌面行为**:
- 窗口关闭按钮 = 隐藏到托盘(`CloseRequested` → prevent + hide);托盘菜单「显示」恢复
- 托盘「退出」= 有运行中子进程时确认 → `stop_all`(逆序优雅)→ daemon shutdown → exit
- 单实例:二次启动聚焦已有窗口(tauri-plugin-single-instance 必须最先注册)
- 开机自启:不做(需求明确不需要 OS service;Tauri autostart 插件留作 P2)

**配置**:(2026-08-17 更新)内嵌 daemon 默认配置文件名独立为 `services.desktop.toml`,查找链 `$WARDEN_CONFIG` → `<exe_dir>/config/` → `<app_data>/config/`(不落 cwd 与 CLI 平台位置,防拾取 CLI 的 services.toml;格式与 CLI 完全兼容);`config_path` 传解析路径,API reload 重读同一文件。data_dir/log_dir 仍重定向桌面应用数据目录。

## 3. daemon 侧扩展(P1 前置,TDD)

**组级启停 API**(服务端做才有"优先级+就绪推进"语义,复用 `start_ordered`):

- `Supervisor::start_group(group)` / `stop_group(group)`:谓词 = `config.group == Some(g)`;启动走就绪推进,停止逆序;desired-state 同步标记
- 路由:`POST /api/v1/groups/{group}/start`、`POST /api/v1/groups/{group}/stop`
- 测试:排序谓词单测 + e2e(同组 stamp 有序、异组不受影响、组停逆序)

group 过滤列表前端本地做(`/services` 已含 group 字段),不加服务端过滤参数(YAGNI)。

## 4. 前端功能清单(P1)

- **节点侧栏**:节点卡片(显示名/地址/健康点/服务数/失败数);本地节点标「内嵌」徽章;添加/编辑/删除远程节点(对话框:url/token/名称);节点级连接失败提示
- **服务区**:组过滤 chips(全部/未分组/各组,含组级「全部启动/停止」按钮)+ 关键词过滤 + 全启/全停;服务卡片:状态徽章/健康点/`[组]`/`P优先级`/PID/CPU/内存/监听端口(tcp:8790 udp:6390)/重启次数;行内操作 启动/停止/重启/编辑/删除(跨节点聚合展示,卡片标注所属节点)
- **日志面板**:选中服务 SSE 实时流(每节点一个 EventSource;token 走轮询降级,与 CLI web 同策略)+ 关键词/等级过滤 + 暂停/清空/自动滚动
- **服务 CRUD 表单**:对齐 CLI web 字段 + group/priority;仅对支持 CRUD 的节点可用(全部 warden 均支持)
- **暗色主题**(对齐 CLI web 风格基调,重新设计)

P2(后续,不在本次):metrics 历史图表、系统通知(健康迁移)、自动发现局域网 warden、前端多语言。

## 5. 风险与决策点

| # | 问题 | 决定 |
|---|---|---|
| R1 | Tauri 编译链较长(首次全量编译数分钟) | 接受;debug 迭代用 vite HMR + `tauri dev` |
| R2 | 内嵌 daemon 随机 token 的传递 | 启动时 Tauri event `warden://ready` 携带 {port, token},前端初始化本地节点 |
| R3 | CLI 版 warden 与桌面版同机并存 | 端口不同(随机 vs 8789)互不冲突;默认配置文件名已隔离(CLI `services.toml` vs 桌面 `services.desktop.toml`,2026-08-17),共用配置需显式 `$WARDEN_CONFIG` 指向同一文件 |
| R4 | 组名含 `/`(路径参数冲突) | 组级 API 用 URL 编码;组名约束在 validate_service 增加(禁 `/`)——CRUD 校验顺带收紧 |

## 6. 实施顺序(P1)

```
1. warden lib 组级启停(TDD:单测→实现→e2e→路由) ✅
2. workspace 改造 + Tauri 脚手架(desktop/,vue 模板,@tauri-apps/cli devDep) ✅
3. Rust 桌面端:单实例 + 内嵌 daemon + 托盘 + 关闭最小化 + nodes.json 命令 ✅
4. Vue 前端:HTTP 客户端 + 节点栏 + 服务区(组过滤/组启停)+ 日志 + CRUD ✅
5. 验证:cargo test 三绿 + tauri build 编译链通过 + 手动启动演示 ✅
   (release 构建 5m04s 产出 target/release/warden-desktop.exe;演示:WARDEN_CONFIG
   注入三件套配置,内嵌 daemon 按优先级拉起 rs-iot→reasonix→gateway,随机端口监听)
```

### P1 实施记录(2026-08-15)

- **组级 API**(warden 主 crate,TDD):`names_in_group`/`start_group`/`stop_group` + 路由
  `POST /api/v1/groups/{group}/start|stop`(desired 同步标记);组名禁 `/`(validate_service)。
  测试 74→+4(names_in_group 单测 1、组 e2e 1、组路由 1、组名校验 1),三绿。
- **桌面端**:crate `warden-desktop`(desktop/src-tauri),path 依赖复用 warden;
  单实例插件最先注册;内嵌 daemon 绑 127.0.0.1 随机端口 + getrandom 随机 token,
  data/log 目录重定向应用数据目录(与 CLI 隔离);托盘(显示/退出),窗口关闭=隐藏;
  退出 = shutdown → stop_all(逆序优雅)→ serve 5s drain → exit。节点注册表 nodes.json。
- **前端**:Vue 3 + Vite(纯 JS,无 TS/pinia),vite build 产出 gzip 34KB;
  节点栏(内嵌自动注入 + 远程 CRUD)/服务卡片(组·优先级·端口·CPU/内存)/
  组 chips(悬停出组启停)/日志(SSE,token 节点轮询降级)/服务 CRUD 表单。
- **验证**:warden 74 测试全绿(见风险栏偶发说明)+ `cargo fmt/clippy -D warnings` clean
  (workspace 全体含 desktop)+ `pnpm tauri build --no-bundle` 成功。
