# warden 桌面版(Tauri 2 + Vue 3)

多节点进程监护管理桌面客户端。方案与设计见仓库 [`docs/PLAN-DESKTOP.md`](../docs/PLAN-DESKTOP.md)。

## 架构

- **Rust 侧**(`src-tauri/`,`path` 依赖复用根 crate `warden`):
  - 进程内起**内嵌 daemon**(监护引擎 + HTTP API),绑 `127.0.0.1` 随机端口 + 随机 token
  - 远程节点注册表持久化(`nodes.json`,应用数据目录)
  - 全局单实例(`tauri-plugin-single-instance`)、托盘(关闭=最小化,托盘退出时优雅停止全部子进程)
- **前端**(`src/`,Vue 3 + Vite):本地内嵌节点与远程节点**统一走 HTTP API**,一套客户端代码
  - 节点侧栏(添加/删除 warden 节点)、服务卡片(状态/健康/组/优先级/CPU/内存/监听端口)
  - 「全部节点」为观察视图:过滤与单服务启停可用,批量动作(新增服务/全部启停/组级启停)禁用——须先选中具体节点
  - 分组过滤 chips + **组级全部启动/停止**(daemon 侧按优先序/逆序执行)
  - 日志面板(SSE 实时流,token 节点自动降级轮询)、服务 CRUD 表单

## 开发

```bash
pnpm install
pnpm tauri dev          # 开发模式(vite HMR + cargo 增量)
pnpm tauri build        # 生产构建(安装包)
pnpm tauri build --no-bundle   # 仅产出 exe
```

内嵌 daemon 的配置文件名独立(`services.desktop.toml`,与 CLI 的 `services.toml` 区分),查找链:
`$WARDEN_CONFIG` → `<exe目录>/config/` → `<cwd>/warden/config/` → 应用数据目录;
启动时把进程 cwd 锚定到配置基准目录,配置内相对路径(`../bin/xxx` 等)与启动方式解耦。
`data_dir`/`log_dir` 固定为桌面应用数据目录(与 CLI 版隔离,防止双 daemon 同写状态文件);
窗口/标题栏名称可经 `[daemon] title` 配置(health 接口透出)。
