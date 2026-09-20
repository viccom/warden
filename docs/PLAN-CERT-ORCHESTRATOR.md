# 实施方案:证书编排器(warden 编排 lego 实现 UI 一键申请/续期证书)

> **状态:立项待实施**(2026-09-20 需求定案并存档,尚未编写任何实现代码)。
> 实施时按 §5 步骤走 TDD,每步独立可验证可停。

## 1. 背景与目标

P3 已实现证书外部托管协同(`src/proxy/tls.rs`:30s mtime 热重载 + 1h 到期
检测告警 + 可选 `renew_command` 钩子;协同流程见
[TESTING-ACME.md](./TESTING-ACME.md)),但**初始配置全靠命令行手动**:
现场要自己装 acme.sh/lego、写凭据、拼命令、配 cron。

本功能让 warden 做**编排器**:检测/安装 lego → Web UI 一键申请证书
(收集 DNS 凭据 → 生成命令 → 触发 → 观测输出)→ 落盘热重载 → 自动续期。
全流程 UI 可视,运维不再碰命令行。

**与 D13 的关系(决策演进,非推翻)**:D13"不内置 ACME/DNS-01 客户端"
原则不变——ACME 协议仍由 lego 外部执行,warden 只做编排(进程管理 +
文件落盘,全部复用既有监护经验)。唯一演进:凭据存放从"不进 warden
任何文件"放宽为"**独立 0600 凭据文件,不进主配置 services.toml**"——
这是编排器可用性的硬前提(自动续期需凭据持久),且凭据与主配置隔离,
导出/备份 services.toml 不泄露 DNS 密钥。

## 2. 范围外(不做,防蔓延)

- ACME/DNS-01 协议实现(仍由 lego 承担,D13);
- acme.sh 的编排/UI 集成(保持 TESTING-ACME.md 手动路线);
- 多根域/多张证书(维持 D10 单根域单证书模型);
- 证书吊销、OCSP、EAB(ZeroSSL 等)——lego 支持但 UI 暂不暴露。

## 3. 已确认决策(2026-09-20,用户定案)

| 编号 | 决策点 | 定案 |
|---|---|---|
| C1 | 编排工具 | **只编排 lego v5+**:单二进制、linux/windows 全平台一致(warden 本身双平台发布)、凭据走 env/--env-file、hook 语义已实测(TESTING-ACME.md 路线 C) |
| C2 | lego 获取 | **检测优先 + 可自动下载**:显式 `lego_path` → `<data_dir>/bin/lego[.exe]` → PATH;找不到则从 GitHub release 下载对应平台二进制(不可达时 UI 回退指引手动放置) |
| C3 | 凭据存储 | **独立凭据文件**:默认 `~/.config/warden/acme.env`(unix 0600,覆盖式写入,不入主配置),lego 经 `--env-file` 消费;API GET 永不回显凭据 |
| C4 | 表单范围 | **通用 env 键值入口**:UI 提供通用"DNS 环境变量"键值 textarea + 常用 provider(腾讯云 tencentcloud/阿里云 alidns/Cloudflare 等)变量名提示;天然支持 lego 全部 150+ provider,零逐家维护 |
| C5 | 自动续期 | **warden 内部驱动**:复用 `spawn_cert_expiry` 1h 周期,剩余天数 < `renew_days` 时触发 lego renew(沿用 24h 成功冷却);lego 编排配置完整时 `renew_command` 被忽略(避免双重驱动) |

实测依据:2026-09-20 于 `*.gxai.site` 真实签发/续期 ×2/deploy-hook 落盘
全通(v5.6.0;三个 v4→v5 破坏性变化与 hook 不走 shell 等细节见
TESTING-ACME.md 路线 C)。

## 4. 设计

### 4.1 API(feature 门控 + 鉴权,照 routes_proxy.rs 模式)

| 方法 | 路径 | 行为 |
|---|---|---|
| GET | `/api/v1/proxy/cert` | 证书状态(exists/SAN/notAfter/days_left)+ lego 状态(installed/path/version/source)+ acme 配置(非敏感字段)+ 最近任务 |
| POST | `/api/v1/proxy/cert/issue` | 一键签发。body:`{email, dns_provider, env:{..}, domains?[], server?}`;domains 缺省从 `[proxy].domain` 推导 `<domain>` + `*.<domain>`;server 缺省 letsencrypt。任务运行中 → 409 |
| POST | `/api/v1/proxy/cert/renew` | 手动续期(`lego run --renew-force ...`);409 同上 |
| POST | `/api/v1/proxy/cert/lego/install` | 自动下载安装 lego;409 同上(安装也是任务) |

错误语义照 routes_proxy:校验失败 `Config`(400)、任务冲突 `Conflict`(409)、
资源不存在 `NotFound`(404),`WResult` + 全员过 `auth_middleware`。

### 4.2 核心模块 `src/proxy/certmgr.rs`(新建,feature 门控,~500 行)

1. **CertTaskManager**:最近一次任务 `{status: Running|Success|Failed, kind: Install|Issue|Renew, started_at, finished_at, exit, output: 环形缓冲 500 行}`;AppState 挂 feature 门控字段(同 `proxy_shared` 先例);同一时间仅一个任务(409)。
2. **任务执行器**:`tokio::process::Command` + `kill_on_drop(true)`(对齐 Y1 修复)+ `BufReader` 按行捕获 stdout/stderr 进缓冲——现有 `run_renew_command` 是全量缓冲"跑完拿全文",观测场景需要流式,新写;超时/编码/退出码语义复用其经验(600s 上限;lego 直调不经 shell)。
3. **resolve_lego**:§3 C2 检测顺序;命中后跑 `lego --version` 确认可执行并取版本。
4. **自动下载**:GitHub releases latest 按目标平台映射资产(linux→`tar.gz`/windows→`zip`,GOOS/GOARCH 取 `std::env::consts`);reqwest stream 下载(**依赖已有**,`features=["stream"]`);**调系统 tar 解包**(Windows 10+ 自带 bsdtar 可解 zip——零新解压依赖);unix `chmod +x`;`lego --version` 验证。
5. **签发/续期编排(`issue`/`renew_once` 共用)**:
   - 前置:写 acme.env(0600,unix `OpenOptionsExt::mode`;windows 尽力而为)→ 若 `persist`(默认 true)同步写回 `[proxy.acme]` 非敏感字段(config_edit 锁内,照 routes_proxy 事务模板:锁 → 校验 → `ConfigFile` → 写回 → save → sync);
   - 执行:`lego run --accept-tos --email .. --server .. --dns <provider> -d .. -d .. --path <data_dir>/lego --env-file <acme.env>`(续期加 `--renew-force`);
   - 成功后:warden 代码把 `<path>/certificates/<主域>.crt/.key` **原子拷贝**(tmp+rename,复用 config_edit 的 atomic_write 经验)到 `cert_file/key_file`(未配置则默认 `<data_dir>/ssl/{fullchain.pem,privkey.pem}` 并写回配置)→ 主动触发 `CertReloader::reload_if_changed`(需把 Arc<CertReloader> 从 spawn_tls_serve 装配处共享到 AppState/编排器)。

### 4.3 配置 schema(`src/config.rs` AcmeConfig +6 字段,serde default 齐全)

```toml
[proxy.acme]
expire_warn_days = 21              # 既有
renew_command = "..."              # 既有(通用兜底;lego 编排完整时被忽略,C5)
# ── 以下为编排器新增 ──
lego_path   = ""                   # 显式二进制路径;空 = 自动检测(§3 C2)
email       = ""                   # ACME 账户邮箱
server      = "letsencrypt"        # shortcode 或 directory URL
dns_provider = ""                  # lego --dns 参数(如 tencentcloud)
env_file    = ""                   # 凭据文件;空 = <config 目录>/acme.env
renew_days  = 30                   # 剩余 < N 天自动续期;0 = 关闭自动续期
```

`services.example.toml` 同步注释(含凭据不进主配置的安全说明)。

### 4.4 Web UI(`web/index.html` 反代管理页顶部新"证书"卡片区)

- 证书状态行:SAN/到期/剩余天数(数据来自 GET cert,2s 轮询节拍已有);
- lego 状态行:未安装 → "自动安装"按钮(触发 install 任务并展示输出);已安装 → 版本+路径;
- "一键申请证书"按钮 → 模态表单(照 openRouteForm 模式):email / DNS provider
  下拉(常用几家 + 自定义)+ 通用 env 键值 textarea(placeholder 给腾讯云等变量名示例)+
  域名预填(从 domain 推导,可改)/ server;
- 任务进度区:状态 + 输出尾部(mono 样式,轮询 GET cert 的 task 字段);
- 手动"立即续期"按钮(存在证书时可用)。

### 4.5 依赖

**零新 crate**:HTTP 下载用既有 reqwest(stream);解压用系统 tar;其余全复用。

## 5. TDD 实施步骤(每步三绿可停)

1. **配置层**:AcmeConfig 新字段 + 解析/默认值单测 + example 注释。
2. **certmgr 核心**:CertTaskManager / resolve_lego / 平台资产映射纯函数 / run_lego 执行器(超时/输出环形缓冲)+ 单测(执行逻辑按"执行器可注入"组织,单测层用内存假执行器,e2e 层仍走真实 spawn——见步骤 3)。
3. **API + 装配**:4 端点 + AppState/路由注册 + **fake-lego e2e**——垫片为**进仓库的 `src/bin/fake-lego.rs`**(std-only ~20 行:`--version` 假输出;`run` 时把预置自签证书拷到 `--path/certificates/`),e2e 经 `env!("CARGO_BIN_EXE_fake-lego")` 取路径(Cargo 原生机制:integration test 前**构建期**自动编译所有 [[bin]] 并缓存,源码不变不重编,测试零编译调用、开箱即用)。**不采用测试运行时 rustc 直调**:绕过 cargo 缓存、Windows 输出名要手工补 .exe 分支;也不采用 .cmd 垫片:Windows 上 std spawn `.cmd` 走 cmd.exe 代理(BatBadBut/CVE-2024-24576 修复分支),参数转义语义与真实可执行不同。[[bin]] 垫片让被 spawn 对象与真实 lego 同为原生可执行、unix/windows 同一份逻辑,进程边界/参数/超时/输出捕获的测试语义与生产一致。代价说明:cargo build 多编一个 std-only 小 bin(不进发布产物——release.yml 只打包 warden(.exe);不影响 desktop,不依赖任何 feature)。覆盖 issue → 执行 → 拷贝落盘 → daemon 热重载(TLS 握手验新证书)全链;CI 不连真 LE。
4. **自动续期**:`spawn_cert_expiry` 接 `renew_once`(lego 编排分支,C5 优先级)+ e2e(自签证书 notAfter 拨到 renew_days 内,断言 fake lego 被调用 + 落盘更新)。
5. **Web UI**:证书卡片区 + 表单 + 任务面板。
6. **文档 + 收尾**:TESTING-ACME.md 补"路线 D:warden 编排(本功能)";ROADMAP 变更日志;双 feature 形态 `cargo fmt --all --check` + `clippy --all-targets --jobs 6 -- -D warnings` + `cargo test --jobs 6` 全绿;真机冒烟:真实下载 lego + `lego --version`。

## 6. 涉及文件

| 文件 | 改动 |
|---|---|
| `src/proxy/certmgr.rs` | **新建**:任务管理器/执行器/resolve/install/编排 |
| `src/config.rs` | AcmeConfig +6 字段 + 校验 |
| `src/api/mod.rs` | AppState + 路由注册(feature 门控) |
| `src/api/routes_proxy.rs` 或新 `routes_cert.rs` | 4 端点 handler |
| `src/lib.rs` | CertTaskManager/CertReloader Arc 装配、spawn_cert_expiry 接线 |
| `src/proxy/tls.rs` | spawn_cert_expiry 加 lego 自动续期分支(或抽到 certmgr) |
| `web/index.html` | 证书卡片区 + 表单 + 任务面板 |
| `config/services.example.toml` | [proxy.acme] 新字段注释 |
| `src/bin/fake-lego.rs` | **新建**:e2e 测试垫片(std-only,仅测试链路;非 warden 发布组件) |
| `tests/certmgr_e2e.rs` | **新建**:fake-lego 全链 e2e(issue/续期/落盘热重载/自动续期接线) |
| `docs/TESTING-ACME.md` 等 | 路线 D/变更日志 |

## 7. 风险与已知边界

- **真实签发验证延后**:LE 同域名重复证书 **5 张/周**限速(2026-09-20 联调已耗 4 张),CI/手测全用 fake lego;真签发演练待限速窗口过后按 TESTING-ACME.md 命令执行。
- **内网下载不可达**:github.com 不可达时 install 失败,UI 回退指引手动放置(检测路径不受影响)。
- **Windows 权限**:acme.env 无法设 0600(ACL 从简),文档注明;下载解包依赖系统自带 tar.exe(Win10 1803+)。
- **续期失败重试**:沿用现有语义——仅成功计 24h 冷却,失败按 1h 周期重试;lego renew 未到期时幂等退出 0,失败重试撞 LE 限速风险已评估可接受(TESTING-ACME.md 已注明 renew_command 需幂等)。
- **凭据安全边界**:凭据经 API 明文传输(warden API 自身有 Bearer token;生产建议 https)+ 0600 落盘 + GET 永不回显;acme.env 被删除时自动续期失败、告警链路(到期检测)兜底。
- **桌面版不涉及**:desktop 依赖 `default-features = false`(D6),天然无此功能。
