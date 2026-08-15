# warden 自升级技术调研

> **状态**:调研完成,取舍待讨论(见 §5 待决问题)。定稿后在本文件登记结论,并更新 ROADMAP。
> **日期**:2026-08-15
> **关联**:[PLAN-GROUP-PRIORITY-PORTS.md](./PLAN-GROUP-PRIORITY-PORTS.md) —— 升级编排的"有序 drain"依赖分组/优先级方案。

## TL;DR

warden 是常驻守护进程(Windows Service / systemd),推荐**用自研 [rs-selfupdater](https://github.com/viccom/rs-selfupdater) 作为更新引擎,但不用其 `update_and_restart()` 的内置重启语义**——由 warden 自己编排"下载校验换文件"与"重启"两个阶段,重启走服务管理器(SCM 恢复策略 / `systemctl restart`)。热升级(零停机)在 Rust 原生生态无成熟路线,不做;用"秒级重启 + 按优先级顺序恢复"达成实际效果。

## 1. 背景与约束(warden 特有的前提)

- **本体生命周期归 OS 服务管理器管**:`service/windows.rs` 注册 SCM,`service/systemd.rs` 注册 systemd。任何"起新进程再退出"的自重启在 SCM 下是错的:SCM 会把服务标记为停止,新进程游离在 SCM 之外,下次开机 SCM 再拉一个就双实例。
- **子进程随本体共死**:每个子进程一个 Job Object(`KILL_ON_JOB_CLOSE`,见 `supervisor/signal.rs`),warden 退出 = 全部被管服务被杀。**升级窗口必然是全栈重启**,设计目标是窗口短、顺序对、失败可退,而非零停机。
- **提权运行 + 网络暴露 API**:升级通道是最高危攻击面,签名校验是硬需求,不是可选项。
- 已有可复用钩子:`VERSION`(`lib.rs`,经 `/api/v1/health` 暴露)、`run_app_with_shutdown` 优雅停机链路、`relaunch_elevated()` 自重启先例(`service/windows.rs`)。

## 2. 候选方案

### 2.1 self_update(jaemk)

- <https://crates.io/crates/self_update> / <https://github.com/jaemk/self_update>
- 定位:GitHub/GitLab/Gitea Releases 全流程自更新(查版本、下载、换文件)。
- 成熟度:**活跃维护**(1.0.0-rc.6,2026-07;累计约 1075 万下载)。
- 短板:更新源绑定 Release 平台形态;**无内置发布产物签名校验**(截至调研日,以官方文档为准)——对提权守护进程是硬伤;换文件与重启语义同样面向 CLI 工具,不面向 SCM 托管服务。

### 2.2 self-replace(mitsuhiko)

- <https://github.com/mitsuhiko/self-replace> / <https://docs.rs/self-replace>
- 定位:底层"运行中的可执行文件替换自己"原语库。
- 成熟度:**稳定**(1.5.0,2024-09 后无需再动;累计约 1139 万下载)。Windows 上利用"运行中的 exe 不能覆盖但可以 rename"的技巧完成交换,`.old` 清理留待下次启动。
- 定性:不是完整更新方案,是**最可靠的换文件地基**。

### 2.3 axoupdater / cargo-dist(axo.dev)

- <https://crates.io/crates/axoupdater> / <https://axodotdev.github.io/cargo-dist/>
- 定位:cargo-dist 安装器(install receipt)配套的自动更新器。
- 短板:warden 不走 cargo-dist 安装器分发,绑定太深;axo.dev 2024 底宣布收缩工具链维护。**排除**。

### 2.4 rs-selfupdater(自研)

- <https://github.com/viccom/rs-selfupdater>(crate 名 `selfupdater`,MIT)
- 定位:跨平台自升级基础库,`self-replace` 之上的完整流程:HTTP(S) `latest.json` manifest(按 os/arch 列资产,url/sha256/size/可选签名)→ `check()` 版本比对 → 下载 + SHA256 强制 + minisign/Ed25519 可选签名(fail-closed)→ 换文件。可选 axum 升级服务端 feature(`/api/version|check|update|progress`)。
- 现状:0.1.x 早期(API 可能变),有 Linux/macOS/Windows CI + 集成测试 + e2e harness;`Source` trait 为同步(reqwest blocking),异步在路线图;rollback 计划中未实现。
- Windows 重启语义:spawn 新进程 → 轮询 2s 健康检查 → 旧进程退出(面向 CLI;**不适合 SCM 托管服务**,见 §1)。Unix:`execv` 原位重启(PID 保持)。

### 2.5 dylib 热重载路线(hot-lib-reloader / libloading + abi_stable)

- <https://github.com/rksm/hot-lib-reloader-rs> / <https://fasterthanli.me/articles/so-you-want-to-live-reload-rust>
- 定位:**开发期** live-programming 工具,作者明示非生产用途。
- 根因限制:Rust 无稳定 ABI(跨 dylib 边界必须 abi_stable 化)、reload 丢 dylib 内状态、全局状态(logger/once_cell)跨 reload 异常、必须把可热换部分隔离成独立 crate。**排除**。

### 2.6 对比

| 方案 | 换文件可靠性 | 签名校验 | 源灵活性 | 适配常驻服务 | 维护 |
|---|---|---|---|---|---|
| self_update | ✅ | ❌ 无 | Release 平台 | ❌ CLI 向 | 活跃 |
| self-replace | ✅✅(地基) | —(不管) | —(不管) | —(不管重启) | 稳定 |
| axoupdater | ✅ | ✅ | cargo-dist 绑定 | ❌ | 收缩中 |
| **rs-selfupdater** | ✅(基于 self-replace) | ✅ minisign 可选 fail-closed | ✅ HTTP manifest | ⚠️ 需解耦重启 | 自研可控 |
| dylib 热重载 | — | — | — | ❌ 非生产 | — |

## 3. 热升级(零停机)的现实性

native daemon 的零停机自升级只有两条路,对 warden 都代价过高:

1. **双进程看门狗模型**(launcher + core):launcher 持 HTTP listener,core 可替换重启。代价:Windows 下 listener 句柄交接需 `WSADuplicateSocket`;Job Object 所有权迁移;模块拆分重构。**不做**。
2. **进程外快速重启**:接受秒级窗口。配合需求 1 的优先级顺序启停,把"全栈重启"变成确定性编排(drain 逆序、恢复正序),实际停机感知可控。**采用此路线**。

若未来确有零停机诉求,可选折中:升级窗口临时关闭 Job Object 的 `KILL_ON_JOB_CLOSE` 让子进程存活,warden 重启后按 PID 重新接管(stdout 已断,日志有缺口)——列为远期备选,本期不做。

## 4. 推荐方案(待确认)

**引擎用 rs-selfupdater,编排归 warden**,升级流程(设 `POST /api/v1/upgrade` 端点,走 Bearer 鉴权):

1. `check()`:`latest.json` 版本 vs `VERSION`;支持只查不装(check 只读,可放宽鉴权)。
2. 下载 + SHA256 + minisign 校验(**生产必须配 `public_key`**,SHA256 只信任 manifest 主机本身)。
3. **有序 drain**:按优先级逆序停止全部服务(复用需求 1 的 `stop_all` 排序)。
4. 换文件:self-replace swap(运行中可完成)。
5. 重启走服务管理器:
   - **Windows**:install 时配置 SCM 恢复策略(`sc failure warden reset= 0 actions= restart/5000`),升级时换完文件干净退出,SCM 自动拉起新版本。无额外活动部件。备选:分离 helper 等 PID 退出后 `sc start`(多一个活动部件,仅在恢复策略不可用时考虑)。
   - **Linux**:`systemctl restart warden`(自身发起,systemd 发 SIGTERM → 现有优雅停机链路)。
6. 验证:重启后 `/api/v1/health` 的 `version` 变化即成功信号。

### rs-selfupdater 侧需要的配套改造(按优先级)

1. **解耦 swap 与 restart**:拆出 `download_and_swap(&release)`(或等价 API),重启语义留给宿主——warden 落地的前置条件。
2. **异步 Source trait**:warden 全 tokio,阻塞 reqwest 在 axum handler 里需 `spawn_blocking` 包裹,能用但别扭。
3. **rollback / 指定版本安装**:至少保留 `.old` 可手动回退(SCM 方案下:换回旧文件 + 重启即回退)。
4. 可选:GitHub Releases 便捷 provider(把 `latest.json` 附到 Release asset 即可兼容现有 manifest)。

## 5. 待决问题清单(讨论入口)

| # | 问题 | 倾向 | 备注 |
|---|---|---|---|
| 1 | 分发源:GitHub Releases vs 内网静态 HTTP + `latest.json` | 均可,倾向 Releases + manifest 附资产 | 边缘现场若无法访问公网则必须内网源 |
| 2 | Windows 重启路径:SCM 恢复策略 vs helper 脚本 | SCM 恢复策略 | install 流程加一步 `sc failure` 配置 |
| 3 | 签名是否强制(无 `public_key` 配置时拒绝升级还是放行) | 生产强制,fail-closed | 与 rs-selfupdater 现有语义一致 |
| 4 | 升级窗口子进程处理:全停后随新 warden 拉起(默认) vs 临时保留 | 全停(确定性) | 保留方案见 §3 折中 |
| 5 | 升级触发方式:仅 API 手动 vs 定时自动检查 | 首期仅 API 手动 | 自动检查(只通知不装)二期 |
| 6 | rs-selfupdater 改造(§4 列表)的排期与 warden 侧适配层的边界 | 库侧解耦,warden 薄封装 | 库 0.1.x,API 变更成本低 |

## 6. 参考链接

- self_update:<https://crates.io/crates/self_update> · <https://github.com/jaemk/self_update>
- self-replace:<https://github.com/mitsuhiko/self-replace> · <https://docs.rs/self-replace>
- axoupdater / cargo-dist:<https://crates.io/crates/axoupdater> · <https://axo-blog.pages.dev/2024/06/axoupdater> · <https://axodotdev.github.io/cargo-dist/>
- 热重载:<https://github.com/rksm/hot-lib-reloader-rs> · <https://fasterthanli.me/articles/so-you-want-to-live-reload-rust> · <https://robert.kra.hn/posts/hot-reloading-rust/>
- Windows 运行中 exe 可 rename 不可覆盖:<https://news.ycombinator.com/item?id=45555749>
