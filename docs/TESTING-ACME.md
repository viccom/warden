# 证书外部托管协同指南(P3)——acme.sh / lego 实战 + warden 整合

warden 不内置 ACME 流程(决策 D13):签发/续期/DNS challenge 一概由外部
程序承担(现场 = 1Panel / acme.sh / lego 任选)。warden 侧职责 =
**消费证书文件 + mtime 热重载 + 检测到期 + 可选触发续期命令**。
三者与 warden 的耦合点只有一对文件路径(`cert_file`/`key_file`),
换工具零改动。本文两条主路线均于 **2026-09-20 在 `*.gxai.site` 真实
签发/续期实测通过**,命令可直接照抄。

## warden 侧职责(工具无关)

```toml
[proxy]
https_bind = "0.0.0.0:443"          # 生产;本机 80/443 被占时用 8443
cert_file = "/path/to/fullchain.pem"
key_file  = "/path/to/privkey.pem"

[proxy.acme]                         # 整段可缺省
expire_warn_days = 21                # 剩余低于此值 → tracing warn + alert_webhook(默认 21)
# renew_command = "/opt/acme/renew.sh"  # 可选兜底:到期告警时 warden 执行
```

三个机制(常量见 `src/proxy/tls.rs`):

| 机制 | 行为 | 观测日志 |
|---|---|---|
| mtime 热重载 | 每 **30s**(`CERT_RELOAD_PERIOD`)比对证书对 mtime,变化即换新 acceptor;坏证书沿用旧配置并 warn | `[proxy] 证书已热重载(<路径>)` |
| 到期检测 | 每 **1h**(`CERT_CHECK_PERIOD`)解析 notAfter,剩余 < 阈值时 warn + webhook;首查在启动 1h 后(周期 sleep 在前) | `[proxy] 证书剩余 N 天` / `[warden] 证书临近过期:...` |
| 续期兜底 | 到期告警(剩余 ≤ 阈值)且冷却(24h)已过时执行 `renew_command`;**仅退出码 0 计冷却**,失败下个周期(1h)重试;成功后走热重载吃新证书 | `[proxy] 续期命令已执行(<cmd>):<输出>` / `续期命令失败` |

`renew_command` 执行语义:unix `sh -c` / windows `cmd /C` 整串执行,
**600s 超时强杀**(`kill_on_drop`,Y1 修复),成功判定只看退出码 0,
stdout/stderr 捕获进日志。因此 renew_command 需幂等且自带限速意识
(如 `acme.sh --renew -d x` 天然满足:未到期直接退出 0);持续失败的
命令会按 1h 周期反复重跑,撞 LE 限速。急验到期可用
`openssl x509 -enddate -noout -in <cert>`。

## 路线 A:1Panel 托管(当前 opc.dongx.site 生产)

证书:`/home/ncpe/sslcert/opc-dongx-site/{fullchain,privkey}.pem`
(Let's Encrypt 通配 `*.opc.dongx.site`,2026-09-09 → 2026-12-08)。

**关键配置(上线前必做)**:1Panel 证书续签设置里,把"续签后同步/部署"
目标指向上述目录(或续签钩子脚本覆写两个 PEM)。否则续签只在 1Panel
自己的存储里落盘,**不会**到达 warden 证书路径 → warden 检测到临近过期
持续告警(这正是检测的存在意义:监控续签链路健康)。

演练清单:1Panel 手动触发续签 → `stat -c '%y %n' <目录>/*.pem` 确认
mtime 更新 → ≤30s 后 warden 日志出现 `证书已热重载` → `curl -v` 查看新
有效期 → 权限基线 privkey 640 且 warden 运行用户可读。

## 路线 B:acme.sh(2026-09-20 实测,gxai.site 生产在用)

acme.sh 纯 shell 实现,Linux/macOS/BSD 原生;**凭据自动持久化**,cron
自动续期,落盘路径一次绑定——Unix 现场首选。

```bash
# 1. 安装(装到 ~/.acme.sh,自动注册 cron:每天 4/10/16/22 点第 53 分检查)
curl https://get.acme.sh | sh -s email=<you@example.com>

# 2. 凭据(腾讯云 SecretId/Key → dns_tencent 插件;注意不是 dns_dp,
#    后者用 DNSPod 老 token。acme.sh 会把凭据持久化到 ~/.acme.sh/account.conf)
export Tencent_SecretId="AKID..."
export Tencent_SecretKey="..."

# 3. 签发(必须显式 --server letsencrypt:acme.sh 默认 CA 是 ZeroSSL)
acme.sh --issue --dns dns_tencent -d "gxai.site" -d "*.gxai.site" --server letsencrypt

# 4. 落盘(一次绑定,续期后自动覆写这两个文件;ECC 签发的后续操作都要 --ecc)
acme.sh --install-cert -d "gxai.site" --ecc \
  --fullchain-file /home/ncpe/sslcert/gxai-site/fullchain.pem \
  --key-file       /home/ncpe/sslcert/gxai-site/privkey.pem
```

实测要点:

- 装完 cron 即生效,续期时间由 **ARI 窗口**自动确定(实测签发后立即排到
  2026-11-18,写入 `~/.acme.sh/gxai.site_ecc/gxai.site.conf` 的
  `Le_NextRenewTimeStr`),无需人工排程。
- 落盘绑定持久化在同目录 conf(`Le_RealFullChainPath`/`Le_RealKeyPath`),
  续期后自动覆写目标文件 → warden 30s 内热重载,**零回调**(reloadcmd 留空)。
- 手动续期演练:`acme.sh --renew -d gxai.site --force`,然后按路线 A 的
  mtime/日志/有效期三步核验。
- 兜底写法:`renew_command = "acme.sh --renew -d gxai.site"`(凭据已在
  account.conf,任意环境直接可用——这是与 lego 的关键差异,见下)。

## 路线 C:lego v5(2026-09-20 实测,Windows 现场首选)

Go 单二进制,官方发 Linux/Windows/macOS/ARM 各架构产物,跨平台一致性
最好;实测版本 **v5.6.0**。**v4 → v5 三个破坏性变化**(都是实测踩到的):

1. **CLI 结构重排**:所有参数收进子命令(`lego run ...`),不再是全局
   flag;v4 的独立 `renew` 子命令并入 `run`(`--renew-force` 强制 /
   `--renew-days N` 按剩余天数判断)。
2. **provider 改名**:`tencent` → `tencentcloud`(v4 名字直接报
   unrecognized);凭据环境变量 `TENCENTCLOUD_SECRET_ID` / `TENCENTCLOUD_SECRET_KEY`。
3. **hook 不走 shell**(源码 `cmd/internal/hook/hook.go`):hook 字符串经
   `strings.Fields` 按空白分词后**直接 exec**——`&&`、引号、`$VAR` 展开
   全部无效;环境变量照传但前缀从 `LEGO_` 改为 **`LEGO_HOOK_`**
   (`LEGO_HOOK_CERT_PATH`/`LEGO_HOOK_CERT_KEY_PATH` 等,见
   `cmd/internal/hook/metadata.go`)。**正确写法:hook 指向脚本**,脚本内
   随便用 shell 特性(Linux `.sh` / Windows `.cmd` 同思路)。

```bash
# 签发(通配域名务必加引号,zsh 下裸 * 会 glob 报错)
lego run --accept-tos --email <you@example.com> --server letsencrypt \
  --dns tencentcloud -d "gxai.site" -d "*.gxai.site" --path /opt/warden/lego

# 落盘脚本 /opt/warden/lego/deploy.sh(chmod +x)
#!/bin/sh
cp "$LEGO_HOOK_CERT_PATH"      /opt/warden/ssl/fullchain.pem
cp "$LEGO_HOOK_CERT_KEY_PATH"  /opt/warden/ssl/privkey.pem

# 续期(cron / Task Scheduler / warden renew_command 均调这一行)
lego run --accept-tos --email <you@example.com> --server letsencrypt \
  --dns tencentcloud -d "gxai.site" -d "*.gxai.site" --path /opt/warden/lego \
  --renew-days 30 --deploy-hook /opt/warden/lego/deploy.sh
```

实测要点:

- **lego 不持久化凭据**(与 acme.sh 的本质差异):续期自动化必须自带
  环境,用 `--env-file /opt/warden/lego/.env`(dotenv)注入两个
  TENCENTCLOUD_* 变量,或放进 deploy/renew 包装脚本里 export。
- `--no-random-sleep` 仅供测试跳过随机延迟;生产自动化**保留默认随机
  sleep**(官方建议,分散 CA 负载)。
- lego 用独立 ACME 账号(按 `--email` 注册在 `--path` 下),与 acme.sh /
  1Panel 互不干扰;但**同一域名不要让两套工具绑同一对落盘路径**(互相覆写)。
- warden 集成(Windows 现场):`renew_command = "cmd /C 配置"` 语义下,
  `renew_command = "C:\warden\lego\renew.cmd"` 指向包装脚本(内含
  env-file 调用 lego + 拷贝)即可,到期检测/热重载链路完全一致。

## 通用注意

- **LE 同域名限速**:重复证书(duplicate certificate)**5 张/周**。
  2026-09-20 实测两工具联调共消耗 4 张——强制续期演练请观察冷却,一周
  内别密集重签。
- 证书有效期 90 天,`expire_warn_days = 21` 给运维留足处置窗口;泛域名
  只能走 DNS-01(HTTP-01 不支持通配),凭据只在 ACME 工具一侧持有,不进
  warden 任何配置。
- **凭据安全**:acme.sh 凭据落 `~/.acme.sh/account.conf`(600);lego 走
  环境变量不落盘但需自备注入。生产建议用 CAM 最小权限子账号(仅 DNS
  解析写权限)而非主账号密钥。
- **多消费者共用一张证书**(OpenResty + warden 等)时,落盘路径只写一份,
  各自热重载互不影响;切勿让两个工具写同一路径。
- 通配证书覆盖所有子域(D10 单根域模型);新增子域无需动证书。
- 证书链验证自检:`openssl verify -CAfile <系统信任库> -untrusted
  <fullchain> <fullchain>`;`s_server` 冒烟需 `-cert_chain` 传中间证书,
  否则 "unable to verify the first certificate" 是工具行为差异而非证书缺陷。
- tmpfs 上同 jiffy 重写文件 mtime 不变(内核时间戳缓存)——只影响测试,
  生产 30s 轮询不受影响。

## 实测记录

| 日期 | 域名 | 工具/版本 | 结果 |
|---|---|---|---|
| 2026-09-20 | `gxai.site` + `*.gxai.site` | acme.sh(master,dns_tencent) | 签发 ✓ → install-cert 落盘 `/home/ncpe/sslcert/gxai-site/` ✓ → ARI 续期自动排程 2026-11-18 ✓ → TLS 握手/链验证 ✓ |
| 2026-09-20 | `gxai.site` + `*.gxai.site` | lego v5.6.0(tencentcloud) | 签发 ✓ → `--renew-force` 续期 ×2 ✓ → 脚本 deploy-hook 落盘 ✓ → 产物 `openssl verify` ✓ |

环境:内网机 10.83.40.196;`gxai.site` 公网权威在 DNSPod(beech/only.dnspod.net),
本机可直连 acme-v02.api.letsencrypt.org。
