# warden 使用手册(AI Agent / 自动化运维版)

> **适用版本**:0.3.0(CLI 发行包,含 reverse-proxy 特性)
> **适用平台**:Linux x86_64(glibc ≥ 2.39)、Windows x86_64
> **本文定位**:面向 AI Agent 与自动化脚本的完整操作说明——命令可直接复制执行,
> 请求/响应样例均为真实捕获(非示意)。人读快速上手见包内 `README.md`;
> 源码开发/桌面版见仓库 `README.md` 与 `CLAUDE.md`。

**约定**:`<WD>` 表示发行包解压根目录;Linux 下二进制为 `./warden`,Windows 下为 `.\warden.exe`。
未特别说明时,`$B` 是 API 基址 `http://127.0.0.1:8789/api/v1`。

---

## 0. 30 秒上手

```bash
# 包根目录下执行(config/services.toml 会被自动发现)
./warden run                      # Windows: .\warden.exe run

# 另开终端验证
curl http://127.0.0.1:8789/api/v1/health
# {"failed":0,"running":1,"services":1,"status":"ok","title":null,"version":"0.3.0"}

curl http://127.0.0.1:8789/api/v1/services | jq '.services[] | {name, state}'
```

浏览器打开 `http://127.0.0.1:8789/` 是内置 Web UI(状态/日志/增删改/配置编辑)。
`Ctrl-C`(Linux 为 SIGINT/SIGTERM)触发优雅停机:先停所有被监护服务,再关闭 HTTP。

---

## 1. 部署与启动

### 1.1 包内容

| 路径 | 说明 |
|---|---|
| `warden` / `warden.exe` | CLI 单文件二进制(含监护引擎、HTTP API、TUI、Web UI) |
| `config/services.toml` | **开箱即用**配置(平台化演示服务,`auto_start = true`) |
| `config/services.example.toml` | **全字段参考**配置(逐字段注释,生产模板) |
| `AGENT-GUIDE.md` | 本手册 |
| `README.md` | 面向人的三分钟上手 |
| `LICENSE` | MIT |

运行时会自动创建(相对配置基准目录,即包根):`data/logs/<服务名>/<日期>.log`(被监护进程日志)、
`logs/warden.log.<日期>`(warden 自身日志)、`logs/proxy-access.log.<日期>`(反代访问日志)。

### 1.2 Linux

```bash
chmod +x ./warden
./warden run                                   # 前台;自动发现 config/services.toml
./warden run --config /opt/app/config/services.toml   # 显式指定
./warden --config xxx.toml run                 # 全局位置亦可(--config 是全局参数)
```

生产建议用 systemd(等价手写 unit):

```ini
[Unit]
Description=warden process supervisor daemon
After=network.target

[Service]
Type=simple
ExecStart=/opt/warden/warden run
WorkingDirectory=/opt/warden
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`sudo ./warden install` 会写 `/etc/systemd/system/warden.service`(ExecStart 指向当前二进制路径,需 root),
之后 `sudo systemctl enable --now warden`;`sudo ./warden uninstall` 删除该文件。

> 兼容性:0.3.0 及更早的 Linux 二进制在 Ubuntu 24.04 构建,**需要 glibc ≥ 2.39**
> (Ubuntu 24.04+/Debian 13+/Fedora 40+/RHEL 10+);后续版本构建基线改为 Ubuntu 22.04
> (glibc ≥ 2.35,覆盖 Debian 12 / RHEL 9 / Ubuntu 22.04)。更老的系统请源码构建:
> `cargo build --release --jobs 6`(Rust ≥ 1.81)。

### 1.3 Windows

```powershell
.\warden.exe run                               # 前台(cmd 或 PowerShell)
.\warden.exe run --config D:\warden\config\services.toml
```

注册为 Windows 服务(开机自启,服务名 `warden`):

```powershell
.\warden.exe install      # 非管理员会自动弹 UAC 提权;等价 sc create warden binPath="<exe> service" start=auto
sc start warden
sc stop warden            # 触发优雅停机
.\warden.exe uninstall    # sc stop + sc delete(同样自动提权)
```

> Windows 下被监护进程以进程组方式启动,停止时发 `CTRL_BREAK`;超时(`graceful_timeout_secs`)后
> 用 Job Object 强杀**整棵进程树**。warden 自身被强杀时,Job Object 也会连带回收子进程。

### 1.4 配置文件查找链(未显式 `--config` 时,按序命中即停)

1. 环境变量 `WARDEN_CONFIG` 指向的文件
2. `<可执行文件所在目录>/config/services.toml` ← **发行包默认走这一级**
3. `<当前工作目录>/config/services.toml`
4. 平台用户配置目录(Linux `~/.config/warden/services.toml`;Windows `%APPDATA%\warden\warden\config\services.toml`)

全部未命中:warden 以**空配置**启动(0 个服务,API 照常可用),首次 CRUD 会创建第 3 级路径的文件。

### 1.5 配置基准目录(相对路径锚点)

warden 启动时把进程工作目录锚定到**配置基准目录**:配置文件在 `<base>/config/` 下则 `<base>`,
否则为其所在目录。配置内所有相对路径(`command`/`working_dir`/`data_dir`/`log_dir`/`config_file`)
都相对该锚点解析——整目录迁移、从任意目录启动、Windows 服务模式(工作目录为 System32)行为一致。

---

## 2. 配置文件参考

配置文件是 TOML,**是服务定义的唯一数据源**:API 的增删改会写回该文件(toml_edit 保注释、原子写),
重启后原样恢复。手改文件后调用 `POST /api/v1/config/reload` 热重载即可,无需重启 daemon。

### 2.1 `[daemon]` 段(warden 自身)

| 字段 | 类型 | 缺省 | 说明 |
|---|---|---|---|
| `api_bind` | string | `127.0.0.1:8789` | HTTP API + Web UI 监听地址;`0.0.0.0:8789` 表示对外 |
| `auth_token` | string | `""` | 非空则启用 `Authorization: Bearer <token>` 鉴权;空 = 不鉴权 |
| `data_dir` | string | `./data` | 被监护进程日志落盘根(`<data_dir>/logs/<服务名>/<日期>.log`);置空串 = 不落盘(仅内存) |
| `log_dir` | string | `./logs` | warden 自身日志目录(按日滚动 `warden.log.<YYYY-MM-DD>`) |
| `alert_webhook` | string? | 无 | 健康状态迁移告警 POST 目标 |
| `env` | table | `{}` | 全局环境变量,注入**所有**被监护进程(服务的 `environment` 同名覆盖) |
| `title` | string? | 无 | 桌面版窗口标题;CLI 忽略 |

### 2.2 `[[service]]` 段(每个被监护进程一段)

| 字段 | 类型 | 缺省 | 约束/说明 |
|---|---|---|---|
| `name` | string | **必填** | 唯一 key;禁 `/ \ : * ? " < > \|`;重复 → 启动时跳过该条 + warn,API create 报 400 |
| `command` | string | **必填** | 可执行文件;相对路径按配置基准目录解析,也支持 PATH 查找(如 `ping`) |
| `args` | string[] | `[]` | 参数数组(不经 shell,无引号/重定向语义) |
| `display_name` | string | `""` | 展示名(TUI/Web/桌面卡片) |
| `description` | string | `""` | 一句话描述 |
| `working_dir` | string? | 继承 | 子进程工作目录 |
| `environment` | table | `{}` | 本服务专属环境变量(覆盖 `[daemon] env`) |
| `auto_start` | bool | `false` | daemon 启动时自动拉起(唯一的重启恢复源) |
| `auto_restart` | bool | `false` | 崩溃后是否自动重启;`false` 时**强制**退化为 `never` |
| `restart` | table | 见 2.3 | 重启退避策略 + 退出模式 |
| `health` | table? | 无 | 健康检查,目前仅 `tcp`(见 2.4) |
| `graceful_timeout_secs` | int | `10` | 发停止信号后等待秒数,超时强杀进程树 |
| `output_encoding` | string? | UTF-8 | `gbk`/`cp936`/`utf-8`;中文 Windows 程序输出乱码时设 `gbk`(不支持 UTF-16) |
| `group` | string? | 无 | 分组标签(纯展示 + 组级启停);**不可含 `/`** |
| `priority` | int | `0` | 数值越小越先启动、越后停止;同值按 name 字典序 |
| `ui_url` | string? | 无 | 管理入口,点亮 UI「打开」按钮;反代 auto 路由的上游地址 |
| `config_file` | string? | 无 | 子进程配置文件路径,点亮 UI「编辑」按钮 + `/config-file` 端点 |
| `proxy` | bool | `false` | 允许经反代域名暴露(auto 路由 = `<subdomain>.<domain>`) |
| `subdomain` | string? | `= name` | 下级域名标签,须匹配 `[a-z0-9]+`(小写化后) |

### 2.3 重启策略与退出语义

```toml
restart = { max_retries = 3, backoff_initial_ms = 1000, backoff_max_ms = 60000,
            backoff_factor = 2.0, restart_window_secs = 60 }
# 或只写关心的字段(TOML dotted-key,缺省字段自动补齐):
restart.mode = "unexpected"          # always(默认)| unexpected | never
restart.expected_exit_codes = [0]    # mode = unexpected 时的白名单;默认 [0]
```

| `restart.mode` | 行为 |
|---|---|
| `always`(缺省) | 任意退出 → 按退避重试 |
| `unexpected` | 退出码在白名单内 → 视作预期退出(状态 `stopped`,不重启,`restart_count` 不变);否则同 `always`。典型用途:**子进程自升级**时旧进程主动 exit 0 |
| `never` | 任意退出都不重启(状态 `failed`)。`auto_restart = false` 会强制此模式 |

退避:`第 n 次延迟 = min(backoff_initial_ms × backoff_factor^(n-1), backoff_max_ms)`;
`restart_window_secs` 窗口内超过 `max_retries` 次 → **熔断**进 `failed`(不再重启,
需人工 `POST .../start`,该调用会重置退避计数)。

> 被信号杀死时退出码记为 `-1` 哨兵,因此信号退出**不命中** `expected_exit_codes` 白名单,走退避重试。

### 2.4 健康检查

```toml
health = { type = "tcp", host = "127.0.0.1", port = 8790, timeout_ms = 2000, interval_secs = 5 }
```

仅对 `running` 服务探测 TCP 可连接性;结果写入状态快照的 `health` 字段
(`status`: `unknown`/`healthy`/`unhealthy`,`consecutive_failures`,`last_error`)。
状态迁移(healthy↔unhealthy)时:记 warn 日志 + 推送到服务日志流 + 若配置 `alert_webhook` 则 POST:

```json
{ "service": "demo", "from": "healthy", "to": "unhealthy", "detail": "connect 127.0.0.1:8790 失败:...", "at": "2026-09-20T06:58:41Z" }
```

健康检查**不会**自动重启服务(重启只由进程退出触发)。

### 2.5 `[proxy]` 段(反向代理,可选)

缺省整段不写 = 不启用。启用后 warden 按 Host 把请求分流到各上游。

```toml
[proxy]
domain       = "example.com"        # 根级域名;auto 路由 = <subdomain>.<domain>
http_bind    = "0.0.0.0:80"         # HTTP 入口(0.0.0.0:8080 走非特权端口)
https_bind   = "0.0.0.0:443"        # TLS 终止入口(与 cert_file/key_file 成对)
cert_file    = "/ssl/fullchain.pem" # 证书链 PEM;换盘后 30s 内 mtime 热重载
key_file     = "/ssl/privkey.pem"
preserve_host = false               # false = 转发时改写 Host 为上游;true = 保留客户端 Host
connect_timeout_ms = 5000           # 上游连接超时(仅约束连接建立)
upstream_ca_file = "/ssl/ca.pem"    # https 上游私有 CA(缺省用内置根)

[proxy.acme]                        # 证书到期检测(不申请证书,只管监控/告警/兜底续期命令)
expire_warn_days = 21               # 剩余天数低于此值 → warn + alert_webhook
renew_command = "acme.sh --renew -d example.com"   # 可选;到期前执行(sh -c / cmd /C,600s 超时,24h 冷却)

[[proxy.route]]                     # 显式路由(可多条);显式优先于 auto
host = "app.example.com"            # 精确 host,或 "*.<domain>" 单层通配;禁路径/端口
to   = "http://127.0.0.1:9000"      # 显式上游 URL;或写 service = "<服务名>"(取该服务 ui_url)
# preserve_host = true              # 可选:本条覆盖段级设置
```

分流规则:显式路由 → 服务的 auto 路由(`proxy = true` 时 `<subdomain>.<domain>` → 该服务 `ui_url`);
均未命中返回 **421 Misdirected Request**;服务未运行返回 **503**,未配 `ui_url` 返回 **502**。
http/https 双入口同配时,HTTP 入口全量 301 跳 https。
路由/域名/`preserve_host` 改动经 `reload` 或路由 API 热生效;**监听地址与证书路径变更需重启 daemon**。

> 泛域名证书申请推荐交给外部 ACME 客户端(acme.sh / lego)完成,DNS-01 签好后 warden 直接消费证书文件
> (30s 热重载 + 到期监控)。完整实战与踩坑见仓库 `docs/TESTING-ACME.md`。

---

## 3. CLI 命令参考

```
warden [--config <path>] <COMMAND>
```

| 命令 | 说明 |
|---|---|
| `run` | 前台运行 daemon(监护引擎 + HTTP API);缺省命令,不带子命令时等价 |
| `tui` | 终端客户端:`--url <API 地址,默认 http://127.0.0.1:8789>` `--token <token>` |
| `install` | 注册为 OS 服务(Linux 写 systemd unit 需 root;Windows 自动 UAC 提权并 `sc create`) |
| `uninstall` | 卸载 OS 服务注册 |
| `service` | 仅供 OS 服务管理器调用的入口(Windows Service dispatch;Linux 上调用会报错) |

**退出码**:正常运行期收到 SIGINT/SIGTERM → 优雅停机后退出 0;监听端口被占用等启动期错误 → 退出 1。

> ⚠️ **配置加载失败(配置文件不存在/语法错)时退出码为 0**,只打印到 stdout/stderr:
> ```
> warden 0.3.0:配置加载失败:config error: 配置文件不存在:/tmp/nope.toml
> 用法:warden run --config <path>,或放置 config/services.toml(示例见 config/services.example.toml)
> ```
> 自动化脚本**必须捕获输出文本**判断启动是否成功,不能只看退出码。

---

## 4. HTTP API 参考

### 4.1 通用约定

- 基址:`http://<api_bind>`(缺省 `http://127.0.0.1:8789`),业务端点前缀 `/api/v1`
- 鉴权:`Authorization: Bearer <daemon.auth_token>`;`auth_token` 为空时全部放行
- **白名单**(无需 token):`GET /`(Web UI 页面)、`GET /api/v1/health`
- 鉴权失败:`401` + **纯文本** body `invalid or missing token`(不是 JSON,勿盲目解析)
- 其余 API 错误统一 JSON:`{"error": "<slug>", "message": "<中文/英文描述>"}`
  - 例外:路径不存在(如 `POST /api/v1/services/x/typo`)返回**空 body 的 404**;
    反向代理自身的错误页(421/502/503)是 HTML,不是 JSON
- 时间格式:RFC3339 **UTC**(`2026-09-20T06:58:41.637414369Z`);换算本地时间需 +8(东八区)
- 服务名出现在路径中:`/api/v1/services/{name}`;分组名同理出现于 `/api/v1/groups/{group}`

### 4.2 端点总表

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/v1/health` | 探活(白名单):服务总数/运行数/失败数 |
| GET | `/api/v1/services` | 服务状态快照列表(不含 command/env 等配置细节) |
| POST | `/api/v1/services` | 新增服务,body = ServiceConfig JSON(写入配置文件,**不启动**) |
| GET | `/api/v1/services/{name}` | 单个服务状态快照 |
| PUT | `/api/v1/services/{name}` | 更新配置(任意状态可改;运行中的进程下次 start/restart 生效) |
| DELETE | `/api/v1/services/{name}` | 删除服务(仅 `stopped`/`failed`;运行中 → 409) |
| GET | `/api/v1/services/{name}/config` | 完整 ServiceConfig(编辑表单预填/改配置前取原值) |
| GET | `/api/v1/services/{name}/config-file` | 读子进程配置文件(未配 `config_file` → 404) |
| PUT | `/api/v1/services/{name}/config-file` | 写子进程配置文件,body `{"content":"...","format":false}` |
| POST | `/api/v1/services/{name}/start` | 启动;已在 `running`/`starting`/`stopping`/`restarting` → **409**。成功即重置熔断计数 |
| POST | `/api/v1/services/{name}/stop` | 停止(幂等,已停止再调仍 200);SIGTERM/CTRL_BREAK → 超时强杀 |
| POST | `/api/v1/services/{name}/restart` | 等价 stop → start |
| GET | `/api/v1/services/{name}/logs?tail=N` | 日志快照(缺省 500 行,内存环形缓冲上限 2000) |
| GET | `/api/v1/services/{name}/logs/stream` | SSE 实时日志流(`event: log`) |
| GET | `/api/v1/services/{name}/metrics` | CPU/内存采样 + 状态 |
| POST | `/api/v1/services/start-all` | 全部启动(按 `priority` 升序 + 就绪推进) |
| POST | `/api/v1/services/stop-all` | 全部停止(按 `priority` 逆序) |
| POST | `/api/v1/groups/{group}/start` | 组级启动(组内按优先级序) |
| POST | `/api/v1/groups/{group}/stop` | 组级停止(组内逆序) |
| POST | `/api/v1/config/reload` | 重载配置文件并增量同步(新增注册/消失的优雅停止后移除/同名替换) |
| GET | `/api/v1/proxy` | 反代视图:engine 状态 + domain/binds + 路由表 + 路由级 metrics |
| POST | `/api/v1/proxy/routes` | 新增路由,body = ProxyRoute JSON |
| PUT | `/api/v1/proxy/routes/{host}` | 更新路由(host 为键,不可改名;改名 = 删旧建新) |
| DELETE | `/api/v1/proxy/routes/{host}` | 删除路由 |

### 4.3 关键端点样例(真实响应)

**`GET /api/v1/services`** —— 状态快照的完整字段(每个服务一项):

```json
{ "services": [ {
  "name": "demo-heartbeat", "display_name": "心跳演示",
  "state": { "state": "running", "pid": 3351980, "started_at": "2026-09-20T06:58:17.809197948Z" },
  "restart_count": 0, "last_started_at": "2026-09-20T06:58:17.809197948Z",
  "last_exit": null,
  "metrics": { "cpu_percent": 0.0, "memory_kb": 2040, "sampled_at": "2026-09-20T06:58:24.073155918Z" },
  "health": { "status": "unknown", "last_check": null, "last_error": null, "consecutive_failures": 0 },
  "listening_ports": [],
  "auto_start": true, "auto_restart": true,
  "restart_mode": "always", "expected_exit_codes": [0],
  "group": null, "priority": 0, "ui_url": null, "config_file": null,
  "proxy": false, "subdomain": null
} ] }
```

`state` 是带 `state` 标签的枚举,取值:

| `state.state` | 附带字段 | 含义 |
|---|---|---|
| `stopped` | — | 未运行(从未启动,或主动停止,或 `unexpected` 模式下预期退出) |
| `starting` | — | 已 spawn,等待就绪 |
| `running` | `pid`, `started_at` | 运行中 |
| `stopping` | — | 已发停止信号,等待进程退出 |
| `restarting` | `attempt`, `next_at` | 退避等待中,将在 `next_at` 重启 |
| `failed` | `reason`, `exit_code`, `at` | 熔断/未启用自动重启而退出;需人工介入 |

**`GET /api/v1/services/{name}/logs?tail=3`**:

```json
{ "name": "demo-heartbeat", "tail": 3, "lines": [
  { "stream": "stdout", "level": "unknown", "ts": "2026-09-20T06:58:19.812768139Z", "text": "heartbeat #2 2026-09-20 14:58:19" }
] }
```

`stream`:`stdout`/`stderr`;`level` 由启发式从行内容识别(`debug`/`info`/`warn`/`error`/`unknown`)。

**SSE 实时日志**(`curl -N` / `EventSource`),每条一帧:

```
event: log
data: {"stream":"stdout","level":"unknown","ts":"2026-09-20T06:58:41.637414369Z","text":"heartbeat #9 2026-09-20 14:58:41"}
```

**写操作响应**均为简短 JSON:`POST .../start` → `{"status":"started","name":"demo-heartbeat"}`;
`stop`/`restart` 同理;`POST /api/v1/config/reload` → `{"status":"reloaded","services":1}`;
组级操作 → `{"status":"group-start done","group":"g","services":["a","b"]}`(组不存在时 `services: []`,HTTP 200,不报错)。

**`POST /api/v1/services` 请求体**(即 ServiceConfig 的 JSON 形式,字段与 §2.2 一一对应,可省略取缺省):

```json
{ "name": "my-app", "display_name": "我的应用", "command": "bin/app.exe",
  "args": ["--config", "config.toml"], "working_dir": ".", "group": "core", "priority": 5,
  "auto_start": true, "auto_restart": true,
  "restart": { "mode": "unexpected", "max_retries": 3, "backoff_initial_ms": 1000,
               "backoff_max_ms": 60000, "backoff_factor": 2.0, "restart_window_secs": 60,
               "expected_exit_codes": [0] },
  "health": { "type": "tcp", "host": "127.0.0.1", "port": 8790, "timeout_ms": 2000, "interval_secs": 5 },
  "graceful_timeout_secs": 10, "environment": { "RUST_LOG": "info" },
  "ui_url": "http://127.0.0.1:8790", "config_file": "app/config.toml",
  "proxy": true, "subdomain": "app" }
```

### 4.4 错误码与处置

| HTTP | `error` | 典型 `message` | 处置 |
|---|---|---|---|
| 400 | `config` | `config error: name 'x' 重复` | 换 name,或先 DELETE 旧条目 |
| 400 | `config` | `config error: service 'x' 的 command 为空` | 补 `command` |
| 400 | `config` | `config error: body.name 'other' 与路径 'x' 不一致` | PUT 时 body.name 必须等于路径 name |
| 400 | `config` | `config error: 配置文件中不存在服务 'ghost' 的条目` | 该服务未注册:PUT 只改已存在的条目,新增用 `POST /services` |
| 400 | `config` | `config error: 路由 'x:8080':host 禁端口...` | 去掉端口/路径,host 只能是域名 |
| 401 | —(纯文本) | `invalid or missing token` | 加 `Authorization: Bearer <token>` |
| 404 | `not_found` | `service not found: nope` | 核对服务名(`GET /services` 取全量) |
| 404 | `not_found` | `service not found: x 未配置 config_file` | 先 PUT 服务配置补 `config_file` |
| 409 | `conflict` | `service 'x' 当前为「running」无法删除,请先停止` | 先 `stop` 再 `DELETE` |
| 409 | `conflict` | `service 'x' 当前为「running」无法启动` | 已在运行;要重启用 `restart`,或先 `stop` |
| 409 | `conflict` | `路由 host 'x' 已存在(...)` | 用 PUT 更新,或换 host |
| 500 | `internal` | `io error: ...` | 检查文件权限/磁盘;配置文件只读时 CRUD 会整体拒绝 |

---

## 5. 运维任务配方(Agent 常用动作)

以下 `$B=http://127.0.0.1:8789/api/v1`,需要在命令后加 `-H "Authorization: Bearer $TOKEN"`(若启用鉴权)。

**5.1 判断 daemon 是否活着**

```bash
curl -fsS $B/health >/dev/null && echo up || echo down       # 白名单,无需 token
```

**5.2 服务起停与状态确认**(`start` 立即返回,进程可能仍在 `starting`;要读状态确认)

```bash
curl -s -X POST $B/services/my-app/restart
sleep 1
curl -s $B/services/my-app | jq -c '{state:.state.state, pid:.state.pid, restart_count, last_exit}'
# 期望:{"state":"running","pid":12345,"restart_count":0,"last_exit":{...}或 null}
```

**5.3 批量/组级操作**

```bash
curl -s -X POST $B/services/start-all
curl -s -X POST $B/services/stop-all
curl -s -X POST $B/groups/core/start
```

**5.4 新增服务(推荐走 API,免手写 TOML 语法)**

```bash
curl -s -X POST $B/services -H 'Content-Type: application/json' -d '{
  "name":"my-app","command":"bin/app.exe","args":["--port","9000"],
  "auto_start":true,"auto_restart":true,"group":"core","priority":10,
  "health":{"type":"tcp","host":"127.0.0.1","port":9000}
}' | jq -c .
# {"name":"my-app","status":"created"}   注意:只写文件 + 注册,不启动 → 再调 /start
```

**5.5 修改服务**(先 GET 原配置 → 改字段 → PUT 全量)

```bash
curl -s $B/services/my-app/config | jq . > /tmp/app.json
# 编辑 /tmp/app.json(勿改 name 字段)
curl -s -X PUT $B/services/my-app -H 'Content-Type: application/json' --data-binary @/tmp/app.json
curl -s -X POST $B/services/my-app/restart        # 运行中改配置需 restart 才生效
```

**5.6 删除服务**

```bash
curl -s -X POST $B/services/my-app/stop
curl -s -X DELETE $B/services/my-app              # {"status":"deleted","name":"my-app"}
```

**5.7 读日志 / 跟随日志**

```bash
curl -s "$B/services/my-app/logs?tail=200" | jq -r '.lines[] | "\(.ts) [\(.stream)] \(.text)"'
curl -sN "$B/services/my-app/logs/stream" | head -50        # SSE;Ctrl-C 断开
# 落盘日志(daemon 重启/日志被冲掉后仍可查):
tail -n 200 data/logs/my-app/$(date +%F).log
```

**5.8 编辑被监护程序的配置文件**

```bash
curl -s $B/services/my-app/config-file | jq -r '.content' > /tmp/app.toml   # {path,exists,format,content}
curl -s -X PUT $B/services/my-app/config-file -H 'Content-Type: application/json' \
  -d "$(jq -nc --rawfile c /tmp/app.toml '{content:$c,format:true}')"
# toml/json 会先校验再落盘(坏内容 400 拒绝);仅当服务配置了 config_file 时可用
```

**5.9 热重载配置文件**(手改 TOML 后)

```bash
curl -s -X POST $B/config/reload
# 新增服务注册;已消失的服务优雅停止后移除;同名服务的新配置下次 start/restart 生效
# [proxy] 路由热更新;监听地址/证书路径变更仍需重启 daemon
```

**5.10 反代路由管理**

```bash
curl -s $B/proxy | jq -c '{enabled,domain,http_bind,routes:(.routes|length)}'
curl -s -X POST $B/proxy/routes -H 'Content-Type: application/json' \
  -d '{"host":"app.example.com","service":"my-app"}'          # 或 {"to":"http://127.0.0.1:9000"}
curl -s -X PUT  $B/proxy/routes/app.example.com -H 'Content-Type: application/json' \
  -d '{"host":"app.example.com","to":"http://127.0.0.1:9001","preserve_host":true}'
curl -s -X DELETE $B/proxy/routes/app.example.com
# 响应带 engine 字段:live=已热生效;restart_required=未启用反代
```

**5.11 停止 daemon(优雅)**

```bash
kill -TERM <warden_pid>      # Linux;Windows 服务模式: sc stop warden;前台: Ctrl-C
```

---

## 6. 诊断与排障

### 6.1 状态快照的读法

| 观察点 | 含义与下一步 |
|---|---|
| `state.state = failed` + `state.reason` | 熔断或未启用自动重启。读 `state.exit_code`;处理根因后 `POST .../start`(会重置退避计数) |
| `last_exit.exit_code = 3` | 最近一次**自然退出**的退出码(非信号) |
| `last_exit.exit_code = null` | 最近一次是被信号杀死(记为 `-1` 哨兵,不命中 `expected_exit_codes`) |
| `last_exit = null` | 从未自然退出过;**主动 stop 不写 `last_exit`**(别据此判断"启动失败") |
| `restart_count` 持续增长 | 进程在崩溃-重启循环中;查日志找崩溃原因,必要时先 `stop` 再修 |
| `health.status = unhealthy` + `last_error` | TCP 探测失败详情(端口没监听/防火墙/服务未就绪) |
| `listening_ports` 为空但服务应监听 | 端口由子进程绑定才可见(含孙进程);启动未完成或绑到了其它 pid 树 |
| `metrics.cpu_percent = 0.0` | 首次采样未到(采样周期内),不是故障 |

### 6.2 常见故障 → 处置

| 现象 | 根因 | 处置 |
|---|---|---|
| 启动即 `Error: bind 127.0.0.1:8789 失败:Address already in use`,退出码 1 | 端口被其它进程(如既有 warden 实例)占用 | `ss -ltnp \| grep 8789`(Windows: `netstat -ano \| findstr 8789`)找占用方;改 `api_bind` 或停掉旧实例 |
| 启动打印"配置加载失败"但**退出码 0** | 配置路径不存在/TOML 语法错 | 捕获输出文本判断;`--config` 传绝对路径 |
| 日志中文乱码 | 子进程输出 GBK | 该服务加 `output_encoding = "gbk"` |
| 服务起来了但 `state` 一直 `starting`,再也到不了 `running` | 进程存活但 warden 已判定其退出(极少见);多为命令立即退出 | 看 `last_exit` 与日志;核对 `command`/`args` 能否独立运行 |
| 停止耗时到 `graceful_timeout_secs` 才结束,日志出现"优雅停止超时,强杀进程树" | 目标程序不响应 SIGTERM/CTRL_BREAK | 调大 `graceful_timeout_secs`,或让目标程序支持优雅退出 |
| 反复重启后进 `failed` | 退避窗口内超过 `max_retries`(熔断) | 修根因 → `POST .../start`;或按需调大 `max_retries`/`restart_window_secs` |
| 子进程自升级后被 warden 反复重启 | 旧进程主动 exit 被视作崩溃 | `restart.mode = "unexpected"` + `expected_exit_codes = [0]` |
| API 返 401 | `auth_token` 已配置但请求没带/带错 | 加 `Authorization: Bearer <token>`;401 是纯文本不是 JSON |
| 反代访问返 421 | Host 未命中任何路由(域名/子域拼写不符) | `GET /api/v1/proxy` 看 `routes`;auto 路由要求目标服务 `proxy = true` 且有 `ui_url` |
| 反代访问返 503 / 502 | 目标服务未运行 / 服务未配 `ui_url` | 启动服务;或在 `[[service]]` 补 `ui_url` |

### 6.3 warden 自身日志

`logs/warden.log.<YYYY-MM-DD>`(与前台 stdout 同源):每请求一行 INFO(`http{method,path}: response status=... latency_ms=...`)、
状态迁移、`[config] xxx 写回 <配置文件>` 审计行、`[auth] 拒绝 ...` 等。排障先看此文件与 `console`。

---

## 7. 安全与部署注意

- **`auth_token` 是最重要的开关**:默认只绑 `127.0.0.1`(本机可达)。一旦 `api_bind` 改为 `0.0.0.0`/对外域名,
  必须设置强 token——**API 等价于任意进程启停能力**(能以任意命令 spawn 进程)。
- token 明文存放于 TOML:配置文件权限收紧(`chmod 600 config/services.toml`);勿提交进版本库。
- Web UI 页面本身在白名单(未鉴权可打开),但页面内所有操作都需要 token(浏览器里粘贴填写)。
- `alert_webhook` 只发不收,注意目标端鉴权;URL 里不要内嵌凭据(会出现在配置文件中)。
- 反代只做 Host 分流,不做 WAF/限流;对外暴露时应在更外层(nginx/OpenResty 等)终止 TLS 与限流。
- 被监护进程的启动身份 = warden 的运行身份;**不要用管理员/root 跑无关业务**(Windows 服务模式默认 LocalSystem)。

---

## 8. AI Agent 操作约定(幂等、禁忌、验收)

**幂等性**

| 动作 | 幂等 | 备注 |
|---|---|---|
| `stop` | 是 | 已停止再 `stop` 仍返回 200 |
| `start` | **否** | 已在 `running`/`starting`/`stopping`/`restarting` → 409(`当前为「x」无法启动`);要重启用 `restart` |
| `restart` | 是 | 内部 stop → start,任意状态可调;`start` 会重置熔断计数 |
| `start-all`/`stop-all`/组级操作 | 是 | 逐个操作并**忽略单个失败**(`let _ =`),恒返回 200;组不存在也返回 200(`services: []`) |
| `config/reload` | 是 | 以文件为准重建 |
| `POST /services`(create) | **否** | 重名返回 400;先 `GET /services` 查存在性,或改用 PUT 覆盖 |
| `PUT /services/{name}`(update) | 是 | 全量覆盖该服务配置;配置文件里没有该条目时返回 400(新增请走 POST) |
| `DELETE /services/{name}` | **否** | 不存在返回 404;重复删除会失败(可容忍) |
| `PUT /proxy/routes/{host}` | 是 | 目标不存在时返回 404(需先 create) |

**禁忌(会破坏一致性或误导判断)**

1. **不要在 daemon 运行时直接编辑 `config/services.toml`**:API 的写回会覆盖你的改动。要手改就先停 daemon,
   或改完立刻 `POST /api/v1/config/reload`(且不要与并发 API 写操作交叉)。
2. **不要只看退出码判断 `warden run` 是否成功**:配置错误时退出码是 0(见 §3)。
3. **不要把 401 当 JSON 解析**(纯文本);反之 400/404/409/500 是 JSON。
4. **不要假设 `stop` 是瞬间的**:先 `stop`,轮询状态变 `stopped`(最多等 `graceful_timeout_secs` + 2s)再 `DELETE`。
5. **不要用 `last_exit` 判断"服务是否正常"**:`last_exit = null` 也可能是"从未自然退出"的正常服务。
6. **不要直接改 `data_dir`/`log_dir` 下正在写的日志文件**(只读)。
7. **删除操作不可逆**(配置文件条目真删);删除前先 `GET .../config` 备份 JSON。

**验收清单(每类动作后的确认命令)**

| 动作 | 验收 |
|---|---|
| 启动/重启服务 | `curl -s $B/services/<n> \| jq -c '.state'` → `state.state = "running"` 且有 `pid` |
| 停止服务 | 状态为 `stopped`,且 `listening_ports` 为空 |
| 新增/修改服务 | `curl -s $B/services/<n>/config` 回读字段一致;`grep -c '<n>' config/services.toml` ≥ 1 |
| 删除服务 | `GET $B/services/<n>` 返回 404;`grep -c '<n>' config/services.toml` = 0 |
| 改配置后 reload | `POST /api/v1/config/reload` 返回 `{"status":"reloaded"}`,`GET /services` 数量符合预期 |
| 反代路由 | `GET $B/proxy` 的 `routes` 含目标 host;`curl -H 'Host: <host>' http://<http_bind>/` 返回上游内容(非 421/503) |
| daemon 优雅停机 | 进程退出、被监护子进程全部回收(`ps` 无残留)、`logs/warden.log.*` 出现"已退出" |
