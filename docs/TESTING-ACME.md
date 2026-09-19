# 证书外部托管协同手测指南(P3)

warden 不内置 ACME 流程(决策 D13):签发/续期/DNS challenge 一概由外部
程序承担(现场 = 1Panel,通用等价 acme.sh / lego)。warden 侧职责 =
**消费证书文件 + 检测到期 + 可选触发续期命令**。本文记录两条路线的协同
全流程与观测方法。

## warden 侧配置

```toml
[proxy]
https_bind = "0.0.0.0:443"          # 生产;本机 80/443 被 OpenResty 占用,实测用 8443
cert_file = "/path/to/fullchain.pem"
key_file  = "/path/to/privkey.pem"

[proxy.acme]                         # 整段可缺省
expire_warn_days = 21                # 剩余低于此值 → tracing warn + alert_webhook
# renew_command = "/opt/acme/renew.sh"  # 可选:到期前 warden 主动执行(sh -c;600s 超时;24h 冷却)
```

warden 侧闭环(两条路线共用):

| 机制 | 行为 | 观测 |
|---|---|---|
| mtime 热重载 | 证书对落盘后 **30s 内**自动换新 acceptor,失败沿用旧证书并 warn | 日志 `证书已热重载(...)` |
| 到期检测 | 1h 周期解析 notAfter,剩余 < `expire_warn_days` 时告警 | 日志 `证书剩余 N 天` / `证书临近过期:...` |
| 续期触发 | 配了 `renew_command` 才执行;成功后再走热重载吃新证书 | 日志 `续期命令已执行(...)` |

> 到期检测首查在启动 1h 后(周期 sleep 在前),急验可用 `openssl x509 -enddate`。

## 路线 A:现场 1Panel 托管(当前生产)

证书:`/home/ncpe/sslcert/opc-dongx-site/{fullchain,privkey}.pem`
(Let's Encrypt 通配 `*.opc.dongx.site`,2026-09-09 → 2026-12-08)。

**关键配置(上线前必做)**:1Panel 证书续签设置里,把"续签后同步/部署"
目标指向上述目录(或续签钩子脚本覆写两个 PEM)。否则 12 月续签只在
1Panel 自己的存储里落盘,**不会**到达 warden 证书路径 → warden 检测到
临近过期持续告警(这正是检测的存在意义:监控续签链路健康)。

演练清单(续期窗口走一遍):

1. 1Panel 手动触发续签(或等自动续签);
2. 确认两个 PEM 的 mtime 更新:`stat -c '%y %n' /home/ncpe/sslcert/opc-dongx-site/*.pem`;
3. ≤30s 后 warden 日志出现 `证书已热重载`;无重启、在途连接不断;
4. `curl -v https://<子域>/` 查看新有效期(`expire date` 字段);
5. 权限基线:`privkey.pem` 建议 root:ncpe 640(warden 以 ncpe 跑,保持可读)。

## 路线 B:裸 acme.sh(通用参考)

DNS-01 签通配证书(不占用 80/443 做 challenge,DNS API 凭据由 acme.sh
持有,不进 warden 任何配置/文档):

```bash
curl https://get.acme.sh | sh -s email=<you@example.com>
# 以 DNSPod 为例(其余 DNS 商见 acme.sh dnsapi 目录)
export DP_Id=<id> && export DP_Key=<key>

acme.sh --issue --dns dns_dp -d "opc.dongx.site" -d "*.opc.dongx.site"
# 落到 warden 证书路径(reloadcmd 留空:warden 自己 mtime 轮询,无需回调)
acme.sh --install-cert -d "opc.dongx.site" \
  --fullchain-file /opt/warden/ssl/fullchain.pem \
  --key-file       /opt/warden/ssl/privkey.pem
```

续期验证:`acme.sh --renew -d "opc.dongx.site" --force` 后重复路线 A 的
第 2–4 步。cron 由 acme.sh 安装时自动注册;若想让 warden 兜底触发,配
`renew_command = "acme.sh --renew -d opc.dongx.site"`(到期前 warden 执行,
冷却 24h,输出进日志)。

## 通用注意

- **多消费者共用一张证书**(OpenResty + warden)时,落盘路径只写一份,
  各自热重载互不影响;切勿让两个工具写同一路径。
- 通配证书覆盖所有子域(D10 单根域模型);新增子域无需动证书。
- LE 有效期 90 天,`expire_warn_days = 21` 给运维留足处置窗口。
