# 实施方案:基于域名的 HTTP/HTTPS 反向代理(reverse-proxy 模块)

> **状态**:**待用户评审确认后实施**。方向性决策经两轮确认(见「已确认决策」);第二轮(2026-09-17)按用户澄清收敛为**单根域 + 单张通配证书**模型。
> **日期**:2026-09-17
> **验证门槛**(CLAUDE.md):每步 `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test` 三项全绿(双 feature 形态);编译命令一律 `--jobs 6`。
> **关联**:监听端口发现(`ports.rs`,service 引用上游时辅助诊断);`ui_url`(auto 路由的上游来源);`rsiot-gateway`(现状里用独立进程做反代的证据,本模块落地后现场可少部署一个进程)。

---

## 1. 背景与目标

warden 监护的每个服务大多有自己的 HTTP 入口(`ui_url`,如 `http://127.0.0.1:8790`),现状只能按 IP:端口访问;现场已出现用独立网关进程(`rsiot-gateway`:前端 + 反代)解决域名访问的需求。本模块把**宿主感知的 L7 域名路由反代**内置进 warden:

- 用户把 `*.sub.example.com`(通配 DNS)解析到 warden 所在机器;
- warden 监听 80/443,按请求的 Host 头查路由表(TLS 终止后取 Host,单证书模型下 SNI 不参与路由),反向代理到对应被监护服务或任意 http/https 上游;
- 与进程监护深度集成:引用服务名的路由,服务停止时返回明确的 503 错误页而非裸 502;
- 独立编译开关(Cargo feature),配置解析与引擎分离,feature 未编译时配置存在仅告警。

## 2. 范围外(本期不做,防蔓延)

- 负载均衡/多 upstream、重试、熔断;
- 多根域/多域名与多张证书(当前设计:单一 `[proxy] domain` + 单张通配证书,第二轮确认 D9/D10);
- 内置 ACME/DNS-01 客户端(第三轮确认 D13:外部 acme.sh 托管,warden 只做检测与可选调用);
- HTTP/3(QUIC)、缓存、gzip 重压缩、WAF;
- 路径重写/前缀剥离(路由只按 Host,不按 path);
- 按需拉起服务(start-on-demand);
- 路由级 CRUD API(本期经配置文件编辑器改路由 + `/api/v1/config/reload` 生效,API 化列 P5);
- UDP/TCP 四层代理。

## 3. 已确认决策

第一轮(2026-09-17):

| # | 问题 | 决策 |
|---|---|---|
| D1 | 总体路线 | **A:内置自研,分阶段**(hyper 流式直传;证书层分期) |
| D2 | 路由模型 | **混合:auto 为主 + 显式覆盖**(auto 参数下沉服务级,`[[proxy.route]]` 显式表可覆盖同域) |
| D3 | 证书环境 | **公网域名 + Let's Encrypt** |
| D4 | P1 透传能力 | **HTTP+SSE + WebSocket + 大文件上传流式**(全部进 P1,WS 作为 P1 的独立子步骤) |

第二轮(2026-09-17,用户澄清收敛):

| # | 问题 | 决策 |
|---|---|---|
| D9 | 域名模型 | **单一根域**(如 `warden.domain.com`),`*.warden.domain.com` 通配 DNS 指向本机;不做多根域/多域名支持 |
| D10 | 证书模型 | **单张通配证书**(`*.warden.domain.com`,Let's Encrypt 支持)覆盖全部下级域名转发;无多证书/SNI 分发 |
| D11 | auto 路由来源 | **从进程管理配置解析**:根域配在 `[proxy] domain`,下级域名经 `[[service]]` 级新增参数暴露(见 4.1),不设独立 auto 路由表 |
| D12 | 推论(证书路线) | 通配证书必须 **DNS-01** 签发(LE 协议:ALPN-01/HTTP-01 签不了通配)→ rustls-acme 出局(不支持 DNS-01),自动化路线 = instant-acme + DNS hook 或 acme.sh 子进程(该开放选择由下方 D13 定案) |

第三轮(2026-09-17,用户定案):

| # | 问题 | 决策 |
|---|---|---|
| D13 | ACME 实现 | **完全外部程序托管**(推荐 acme.sh,lego 等同理)——warden **不内置 ACME/DNS-01 客户端**,职责收窄为**检测**(证书到期告警 + mtime 热重载)与**可选调用**(`renew_command` 钩子,到期前触发外部续期命令);D12 的 instant-acme 路线随之出局。**现场落地 = 1Panel**(2026-09-17 确认:申请/续签/DNS API 均由 1Panel 承担,凭据不进 warden 任何文件) |

实现层取舍(按「可逆的实现细节自行决定并说明」):

| # | 问题 | 取舍 | 理由 |
|---|---|---|---|
| D5 | 转发实现 | hyper-util legacy client 流式直传(不用 reqwest) | reqwest 会缓冲/WS 语义不符;hyper-util 已是 axum 传递依赖,增量小 |
| D6 | feature 默认值 | `reverse-proxy` 默认**开**;desktop 显式 `default-features = false` | 发布用户零门槛;桌面内嵌 daemon 不占桌面机 80/443 |
| D7 | 未知 Host | 421 Misdirected Request(不 fallback) | 防被当开放代理打到内网服务 |
| D8 | 配置解析与 feature 关系 | `[proxy]` 段**无条件解析**(纯 serde 结构,零重依赖),引擎 feature 门控;feature off + 配置存在 → 启动 warn 并忽略 | 配置层无 cfg 分叉,两种编译形态解析行为一致,单测可共用 |

## 4. 设计

### 4.1 配置 schema(`[proxy]` 在 `src/config.rs` 无条件解析;auto 参数下沉 `src/model.rs` `ServiceConfig`)

```toml
[proxy]                          # 段存在 = 启用(feature 已编译);当前设计面向**单一根域**
domain = "opc.dongx.site"        # 根级域名:auto 路由 = <subdomain>.<domain>
http_bind  = "0.0.0.0:8080"      # 可选;缺省/空 = 不监听。生产目标 80;本机 80 被 OpenResty 占用 → 8080
https_bind = "0.0.0.0:8443"      # P2 起;生产目标 443,本机 443 同被占用,实测 8443 空闲(2026-09-17)
connect_timeout_ms = 5000        # 上游连接超时(缺省 5000)
preserve_host = false            # 全局缺省:转发时 Host 重写为上游;true = 保留客户端 Host

[[proxy.route]]                  # 显式路由保留原设计(host 完整写,可指向任意 http/https 上游)
host = "fs2.warden.domain.com"   # 精确 host;或 "*.warden.domain.com"(单层通配)
to = "http://127.0.0.1:9000"     # 显式上游;或 service = "rs-iot" 引用被监护服务
```

auto 路由参数在 `[[service]]` 级——**下级域名的路由从进程管理配置解析**(用户第二轮确认 D11):

```toml
[[service]]
name = "fs"
proxy = true                     # ① 允许该服务经反代域名暴露(默认 false)
# subdomain = "fs2"              # ② 下级域名标签:无需写完整域名,约束 [a-z0-9]+;缺省 = name
ui_url = "http://127.0.0.1:8790" # ③ 反代上游(兼作 UI「打开」入口,http/https 均可)
```

TLS(P2 起一张通配证书,全部域名共用,D10):

```toml
[proxy]
# ... domain = "opc.dongx.site" / http_bind 等,见上 ...
https_bind = "0.0.0.0:8443"         # 本机 443 被 OpenResty 占用;生产目标 443
cert_file = "/home/ncpe/sslcert/opc-dongx-site/fullchain.pem"  # 1Panel 托管的 LE 通配证书(与 OpenResty 同用一张,无冲突)
key_file  = "/home/ncpe/sslcert/opc-dongx-site/privkey.pem"
# P3(可选,外部托管,warden 只检测与调用,见 D13):
# [proxy.acme]
# expire_warn_days = 21              # 剩余天数低于此值 → 告警(tracing + alert_webhook)
# renew_command = "..."              # 可选:到期前 warden 主动执行;当前现场 1Panel 自动续签已闭环,无需配置
```

校验(config::validate 同风格,坏项跳过并 warn):`to`/`service` 二选一且必填其一;`to` 必须是合法 http/https URI;`host` 小写化存储、禁 path 部分;通配仅允许前缀 `*.` 且只匹配**单层**子域;重复 host 报错;`http_bind` 与 `https_bind` 均空 → warn(配了 proxy 却无监听);`service` 引用的服务不存在 → warn(先写路由后建服务,reload 生效);`proxy = true` 的服务 `ui_url` 缺失 → warn(该服务不进 auto 路由);`subdomain` 缺省取 `name`,小写化后不满足 `[a-z0-9]+` → warn(不暴露)。

### 4.2 模块布局(`src/proxy/`,feature 门控)

```
src/proxy/
├── mod.rs        # 编排:从 [proxy] 构建引擎、绑定监听器、挂 CancellationToken 优雅停机
├── router.rs     # HostRouter:host 匹配核心(纯函数,单测重灾区)+ auto 服务名解析
├── forward.rs    # hyper-util 转发:头处理、流式直传、WebSocket 隧道
└── tls.rs        # P2/P3:rustls 单证书 acceptor + mtime 热重载;P3 视路线加 acme.rs(DNS-01)
```

### 4.3 路由匹配(确定性顺序)

请求 Host 规范化:**转小写 + 剥离 `:port`**,然后按序:

1. `[[proxy.route]]` **精确 host** 命中 → 用该路由;
2. `[[proxy.route]]` **通配 host**(`*.warden.domain.com` 匹配 `x.warden.domain.com`,单层)命中 → 用该路由;多条通配命中取**最长后缀**;
3. **auto**:host 形如 `<sub>.<domain>`(`domain` 为 `[proxy]` 根域,单层)且 `sub` 命中某 `proxy = true` 服务的 `subdomain`(缺省 name)→ 上游 = 该服务 `ui_url`(请求时经 `snapshot` 解析,ui_url 更新即时生效);
4. 全不中 → **421**。

通配证书与 auto 的层级天然对齐:`*.warden.domain.com` 证书只覆盖一层子域,auto 路由也只暴露一层——多级子域(如 `a.b.warden.domain.com`)两者都不支持,当前设计明确不做。

auto 分支的运行时语义(**显式 `service` 路由同样适用**):服务存在但 `ui_url` 未配置 → 502 页面(配置错误,页面说明"服务 X 未配置 ui_url");服务存在但状态非 Running → **503 页面(含服务当前状态)**——这是 supervisor 反代的差异化能力;服务 Running 但连接失败 → 502。仅 auto 分支多一种:标签不对应任何已暴露服务 → 404 页面("未知服务")。

### 4.4 转发层(`forward.rs`)

- **客户端**:`hyper_util::client::legacy::Client`(连接池,HTTP/1.1;上游为 https 时挂 hyper-rustls,P2 起启用)。
- **请求头**:剥离 hop-by-hop(`Connection` 及其令牌、`Keep-Alive`、`Proxy-Authenticate`、`Proxy-Authorization`、`TE`、`Trailer`、`Transfer-Encoding`、`Upgrade`);`Host` 重写为上游 authority(`preserve_host = true` 时保留原值);**追加** `X-Forwarded-For`(已有则以 `, ` 续接客户端 IP)、设置 `X-Forwarded-Proto`/`X-Forwarded-Host`。
- **响应头**:剥离 hop-by-hop 后透传;`Content-Length`/`Transfer-Encoding` 不手抄,帧重组交给 hyper(避免双 framing)。
- **流式硬性要求**:请求/响应 body 全程 `Body` 流式透传,零缓冲、零落盘——SSE 分块即时下发、大上传不过内存。请求与响应都不设总时长超时(会误杀 SSE/大文件),只约束连接建立(`connect_timeout_ms`)。
- **http/https 特性兼容清单**(用户第二轮要求"尽可能兼容 http/https 相关特性"):`X-Forwarded-Proto` 按 TLS 终止与否设置 http/https;chunked/keep-alive 由 hyper 两侧自动协商透传;上游可为 http 或 https(P2 起 https 上游经 hyper-rustls);TLS 终止后明文转发上游;响应 `Location` 等绝对 URL 不做重写(标准反代行为,应用侧靠 `X-Forwarded-*` 或 `preserve_host` 自适应)。
- **WebSocket**:请求含 `Upgrade: websocket` 时走隧道分支——不经连接池,对上游新建专用连接(https 上游 P2 起支持),透写升级请求后 `hyper::upgrade::on` + `tokio::io::copy_bidirectional` 双向复制;隧道无超时;shutdown 时随 drain 上限强制关闭。
- **access log**:每请求一行 INFO(复用 `log_response` 的模式:method/host(注意代理层 span 用 host 而非 path,path 可能含敏感查询串,记录 path 不记录 query)/status/latency/upstream)。

### 4.5 与 warden 的集成点

- **编排**:`serve_with_shutdown` 内,`[proxy]` 段存在且 feature 已编译时 `proxy::spawn(cfg.proxy, supervisor.clone(), shutdown.clone())`;proxy 自持监听器与 task 组,`shutdown.cancelled()` → 停止接受新连接 → 在途请求 drain(proxy 独立上限 15s,覆盖 WS 隧道强关;API 侧维持现有 5s 不变)。
- **热重载**:`/api/v1/config/reload` 重建路由表(路由对象 `ArcSwap`/重建 engine 持有);`http_bind`/`https_bind` 变更需重启(检测到变更时 warn,不自动重绑)。
- **上游解析**:显式 `service` 与 auto 路由经 Supervisor 查服务名 → `ui_url`;不依赖端口发现(`listening_ports` 仅辅助人工诊断)。
- **配置唯一数据源**:路由表在 `services.toml` 内,经现有配置文件编辑器(Web UI)修改;本期不加路由 CRUD API(范围外)。
- **错误处理**:代理路径的错不进 `WardenError`(那是 API 域的);错误页由 proxy 模块自带极简 HTML/文本(含 host、上游、原因、服务状态)。

### 4.6 依赖与编译开关(Cargo.toml)

```toml
[features]
reverse-proxy = ["dep:hyper", "dep:hyper-util", "dep:http-body-util"]
# P2 追加: rustls, tokio-rustls, hyper-rustls;P3 追加: x509-parser(到期检测);测试 dev-dep: rcgen + axum(ws)

[dependencies]
hyper = { version = "1", optional = true, features = ["client", "http1", "server"] }
hyper-util = { version = "0.1", optional = true, features = ["client", "client-legacy", "http1", "tokio"] }
http-body-util = { version = "0.1", optional = true }
```

- desktop(`desktop/src-tauri/Cargo.toml`)对 warden 的 path 依赖加 `default-features = false`(D6)。
- CI(`ci.yml`)加一个 `--features reverse-proxy` 的 build+test job(windows + ubuntu 各一);无 feature 形态的既有 job 保证回归(D8 的另一半保障)。
- feature off + `[proxy]` 配置存在 → `serve_with_shutdown` 启动时 warn 一行"reverse-proxy feature 未编译,忽略 [proxy] 配置"。

## 5. TDD 实施步骤(分阶段,每阶段独立可验证可停)

### P0 骨架:配置解析 + feature 门控

- [ ] **1. Red:config/model 单测**(双 feature 形态共用):`parse_proxy_section`([proxy] domain/http_bind/route 各字段正确)、`default_proxy_absent`(无段 → None)、`validate_proxy`(to/service 二选一、host 规范化、通配单层、重复 host、非法 to URI);`service_proxy_fields`(`proxy`/`subdomain` 解析、缺省 false/None、subdomain 缺省取 name、序列化 round-trip 不丢字段——CRUD 保障,对齐 group/priority 先例)。
- [ ] **2. Green**:`ProxyConfig`/`ProxyRoute` 结构 + 解析 + 校验(`src/config.rs`,纯 serde 零重依赖);`ServiceConfig` +`proxy: bool`/`subdomain: Option<String>`(`src/model.rs`,serde default,对齐「新增配置字段」约定)。
- [ ] **3. feature 骨架**:`Cargo.toml` `[features]` + 空 `src/proxy/mod.rs`(`#[cfg(feature)]` 挂到 lib.rs)+ serve 集成点(段存在 → 调 `proxy::spawn`,当前为空实现)+ feature off warn 分支。**验证**:双 feature 三项全绿;`cargo check`(无 feature)确认零新依赖。

### P1 HTTP 反代 MVP(含流式/大上传)

- [ ] **4. Red:路由匹配单测**(`src/proxy/router.rs` 内联):精确优先于通配、通配最长后缀、auto 兜底、host 小写/剥端口、未知 421、通配不跨多层、auto 非 Running 服务的 503 语义映射(纯函数:`resolve(host) -> Decision{Route, AutoService(name), NotFound}`)。
- [ ] **5. Green**:HostRouter + auto 服务名解析(查 Supervisor snapshot)。
- [ ] **6. Red:转发 e2e**(`tests/proxy_e2e.rs`,文件头 `#![cfg(feature = "reverse-proxy")]`;本地上游用 `tokio::spawn` + axum 随机端口):
  - `proxies_get_body_and_adds_forwarded_headers`:上游回显收到的头 → 断言 body 一致 + `X-Forwarded-*` 正确 + hop-by-hop 已剥离;
  - `streams_sse_without_buffering`:上游延迟逐条发 3 个事件 → 客户端逐条按时延到达(非一次性);
  - `streams_large_request_body`:上传 8 MiB → 上游收满且哈希一致(零缓冲由 SSE 用例 + 实现结构保证);
  - `returns_421_for_unknown_host` / `returns_502_when_upstream_refused`;
  - `explicit_route_overrides_auto` / `wildcard_matches_single_label`;
  - `auto_service_stopped_returns_503_with_state`(注册 `proxy = true` + ui_url 指向测试上游的服务,未启动,页面含状态文案);
  - `auto_uses_subdomain_override`(subdomain 覆盖 name 的解析与路由)。
- [ ] **7. Green**:`forward.rs`(头处理 + 流式直传 + 错误页)+ `mod.rs` 监听/优雅停机集成。**验证**:全量 `cargo test --features reverse-proxy` + 无 feature 全量。
- [ ] **8. WebSocket 子步骤**:
  - Red:e2e `proxies_websocket_echo`(上游 axum ws 回显,dev-dep 加 axum `ws` feature)——握手经代理、双向消息、断开传播;
  - Green:upgrade 隧道分支(`hyper::upgrade::on` + `copy_bidirectional`)。
- [ ] **9. 收尾**:example 配置加 `[proxy]` 注释段;CI feature job;desktop `default-features = false`。**验证**:双形态三项全绿。

### P2 TLS 终止(单张通配证书)

- [ ] **10. Red:TLS e2e**:rcgen 自签**通配证书**(dev-dep,`*.warden.domain.com`)→ HTTPS 经代理访问 auto/显式路由 → rustls 客户端(`dangerous` 接受自签)断言 body;HTTP→HTTPS 301 跳转(80 与 443 同配时)。
- [ ] **11. Green**:`tls.rs`(tokio-rustls acceptor,单证书;**证书 mtime 轮询热重载**,30s 周期——为续期换证不停机铺路,P3 两条路线共用);https 上游支持(hyper-rustls)。**验证**:三绿 + `curl -k` 手工冒烟(记录于本文档实施记录)。

### P3 证书到期检测 + 外部托管协同(1Panel / acme.sh)

ACME 全流程由外部程序承担(现场 = 1Panel,通用等价 acme.sh/lego;签发/续期/challenge 一概不在 warden 内,第三轮确认 D13)。warden 侧职责 = **检测 + 可选调用**,体量大幅收窄:

- [ ] **12. 证书到期检测 task**:后台周期(1h)解析 `cert_file` 的 notAfter(x509-parser,feature 门控依赖);剩余 < `expire_warn_days`(缺省 21)→ tracing warn + LogHub + 复用 `alert_webhook`(对齐 health 告警模式);每次检测 INFO 一行剩余天数(可观测)。
- [ ] **13. 可选续期调用**:`renew_command` 配置时,到期前触发执行(带超时与日志捕获,复用 warden 进程管理经验);未配置则完全依赖外部托管侧自续期——两种形态都靠 P2 的 mtime 热重载生效,**warden 侧闭环不变**。当前现场(1Panel 自动续签)无需配置;到期检测的实际价值 = **监控 1Panel 续签链路健康**(若续签未同步到 warden 证书路径,11 月中旬起告警)。
- [ ] **14. 手测文档**:`docs/TESTING-ACME.md` 记录**外部托管协同全流程**——现场以 1Panel 为主(续签同步目录配置、warden 到期检测/热重载观测、12 月续期演练);裸 acme.sh 场景(安装、DNS API 凭据、签发 `*.<domain>` 通配、`--install-cert` 落 warden cert 路径 + 可选 reloadcmd、续期验证)作为通用参考。

### P5 可选增强(按需另行立项)

路由 CRUD API、Web UI 路由管理页、路由级 metrics(5xx 计数)、access log 落盘轮转。

## 6. 涉及文件

| 文件 | 改动 | 阶段 |
|---|---|---|
| `Cargo.toml` | `[features] reverse-proxy` + 门控依赖 + dev-dep(rcgen/axum ws) | P0 |
| `src/config.rs` | `ProxyConfig`/`ProxyRoute` + 校验 + 单测(无条件编译) | P0 |
| `src/model.rs` | `ServiceConfig` +`proxy`/`subdomain` 字段(serde default + round-trip) | P0 |
| `src/lib.rs` | `#[cfg(feature)] pub mod proxy` + `serve_with_shutdown` 集成 + feature off warn | P0 |
| `src/proxy/{mod,router,forward}.rs` | 引擎 | P1 |
| `tests/proxy_e2e.rs` | e2e(`#![cfg(feature)]` 门控) | P1 |
| `desktop/src-tauri/Cargo.toml` | warden 依赖 `default-features = false` | P1 |
| `.github/workflows/ci.yml` | +`--features reverse-proxy` job | P1 |
| `config/services.example.toml` | `[proxy]` 注释示例 | P1 |
| `src/proxy/tls.rs` | rustls 单证书 acceptor + mtime 热重载 | P2 |
| `src/proxy/tls.rs`(检测 task)+ `docs/TESTING-ACME.md` | 到期检测/告警 + 可选 renew_command + acme.sh 托管流程文档 | P3 |
| `docs/TESTING-ACME.md` | ACME staging 手测指南 | P3 |

## 7. 风险与开放问题

- **帧重组细节**:hop-by-hop 剥离 + 不手抄 Content-Length/TE 是已知正确范式,但真实上游组合(老设备/非标服务)可能有边角——e2e + 现场 curl 冒烟兜底,发现即补用例。
- **WS + https 上游**:`copy_bidirectional` 隧道对 TLS 上游在 P2 才有 hyper-rustls 支持,P1 期 WS 仅 http 上游(当前服务 argus/safe_bot 均为 http,满足)。
- **外部脚本依赖(D13 的代价)**:证书申请/续签/DNS API 属外部托管侧(现场为 1Panel)职责,warden 只能观测结果(到期检测兜底告警);托管侧续期失败的表现 = 证书临近到期告警,需运维响应。
- **1Panel 续签同步(现场关键路径,2026-09-17 核实)**:证书目录 `/home/ncpe/sslcert/opc-dongx-site/` 是 09-09 单次落盘;若 1Panel 未配置"续签后同步/执行脚本"覆写该目录,**12 月续签不会落盘 → 证书过期**。两道防线:P3 到期检测(11-17 起告警)+ 部署侧在 1Panel 配置续签钩子(见 §8 待办)。
- **私钥权限(现场)**:`privkey.pem` 当前 root:root 644,共享机器上偏宽;建议收紧为 root:ncpe 640(warden 以 ncpe 用户跑,需保持可读)。
- **P3 落地前的空窗**:手动通配证书每 90 天一换(靠 P2 热重载不停机换证;P3 到期检测上线前用日历提醒)。
- **Linux 特权端口**:`systemd --user` 跑 warden 时 80/443 需 `setcap cap_net_bind_service=+ep` 于二进制或 `sysctl net.ipv4.ip_unprivileged_port_start=0`(部署文档化;Windows 无此问题,但首次监听有防火墙放行弹窗)。
- **DNS 前提**:通配路由需用户自配 `*.sub.example.com` A 记录指向机器——属部署文档内容,不是代码能解决的。
- **proxy drain 与大上传在途请求**:15s 上限到了强断(优雅与确定退出优先,对齐既有 5s drain 哲学);文档记录。

## 8. 部署注意事项(实施完成后进 README 部署节)

### 现场环境(2026-09-17 核实)

- 根域 **`opc.dongx.site`**;`*.opc.dongx.site` 通配 DNS → 10.83.40.196(本机);域名托管腾讯云 DNSPod,DNS API 凭据由 1Panel 持有(**warden 不需要,凭据不进任何文件/文档**)。
- 证书:`/home/ncpe/sslcert/opc-dongx-site/{fullchain,privkey}.pem`,Let's Encrypt 通配 `*.opc.dongx.site`,有效期 2026-09-09 → **2026-12-08**;1Panel 申请并托管续签。
- **端口占用(实测)**:80/443 已被 1Panel 部署的 OpenResty(Docker)占用且**不可停**(它也在用同一张通配证书服务既有站点)。warden 本机测试绑**非标端口 8080(http)/ 8443(https)**,访问形如 `https://fs.opc.dongx.site:8443`——通配 DNS/证书均覆盖;Host 路由与端口无关(§4.3 剥离 `:port` 后匹配)。生产/其他边缘机仍以 80/443 为目标形态。
- **与 OpenResty 共存的两种形态**(warden 侧零差别,仅端口配置):① 直接暴露(当前):warden 绑非标端口,客户端 URL 带端口;② 标准端口收口(可选,纯 OpenResty 侧配置):OpenResty stream 按 SNI 透传、或 http 按子域把部分子域反代到 warden 的 8080/8443——若 OpenResty 已终止 TLS,warden 只跑 http_bind 即可。
- P2 直接消费该证书路径(mtime 热重载,续签落盘后 30s 内生效);P3 到期检测自 11-17(剩余 21 天)起告警,是 1Panel 续签链路的监控兜底。
- **上线前待办(部署侧)**:① 1Panel 证书续签设置中配置"续签后同步到该目录"(或续签后执行脚本覆写两个 PEM),否则 12 月续签不落盘;② `privkey.pem` 权限收紧(root:ncpe 640)。

### 通用注意事项

1. 通配 DNS 解析(`*` A 记录 → 机器公网 IP;内网场景改 hosts 或内部 DNS);
2. Linux:setcap / sysctl 二选一;防火墙放行 80/443(生产/标准端口形态;非标端口形态如本机 8080/8443 > 1024,无需 setcap,放行对应端口即可);
3. Windows:防火墙首监听放行;`[proxy]` 段 + `http_bind` 配置;
4. 证书侧:acme.sh 安装于部署机(或任意可达主机),`--install-cert` 落到 warden 的 cert/key 路径;DNS-01 不要求 80/443 为 challenge 开入站,443 仍需入站供用户访问;
5. 与现存 `rsiot-gateway` 的迁移:两者可并存(不同端口),逐步把 gateway 的反代职责迁到 `[proxy]` 后停用该服务。
