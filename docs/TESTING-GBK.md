# GBK 输出解码 —— 手动测试指南

> 验证 warden 的 per-service `output_encoding` 解码能力:被监护进程输出 GBK/CP936 时,设了 `output_encoding = "gbk"` 的服务中文正确解码,不设则乱码。

## 前置工件

| 文件 | 作用 |
|---|---|
| `tests/helpers/gbk_target.rs` | 测试程序:向 stdout 输出 **GBK 编码**中文行(绕过 `println!` 的 UTF-8,模拟中文 Windows 控制台程序),每 0.5s 一行,无限循环 |
| `config/services.test.toml` | 测试配置:两个服务跑**同一个** `gbk_target`,唯一差异是是否设 `output_encoding`,用于对比。端口 `8791`(避开日常 `8789`) |

> ⚠️ **必须用 Git Bash 测试,不要用 cmd/PowerShell**。warden 返回 UTF-8 JSON,只有 Git Bash 终端能正确显示中文;cmd 是 GBK 终端会二次乱码。

## 步骤

### 1. 编译(生成 warden + gbk_target)
```bash
cd E:/github.com/warden
cargo build
```
验证:无报错,`target/debug/gbk_target.exe` 存在。

### 2. 前台启动 warden(开**第一个终端**,保持运行)
```bash
cd E:/github.com/warden
cargo run -- run --config config/services.test.toml
```
预期:
```
[warden] 0.1.0 启动,配置 2 个服务,api_bind=127.0.0.1:8791
[warden] HTTP API listening on 127.0.0.1:8791
```

> 💡 **更直观的方式:浏览器打开 <http://127.0.0.1:8791/>**
> 左侧点 `gbk-decoded` 看中文实时滚动(SSE),点 `gbk-raw` 看乱码对比;顶栏可填 token、行内 start/stop。
> 下方的 curl 步骤是等价操作,**二选一**即可(想直观看解码效果,推荐浏览器)。

### 3. 健康检查 + 服务列表(开**第二个终端**)
```bash
curl -s http://127.0.0.1:8791/api/v1/health | jq
curl -s http://127.0.0.1:8791/api/v1/services | jq
```
预期:`running=2`,两个服务都 `running`。

### 4. 🔑 核心对比:解码效果

**对照组 `gbk-raw`(应乱码)**:
```bash
curl -s "http://127.0.0.1:8791/api/v1/services/gbk-raw/logs?tail=8" | jq -r '.lines[].text'
```
预期:只有 `gbk_target ready` 可读,中文行是 `���...` 乱码。

**实验组 `gbk-decoded`(应正确中文)**:
```bash
curl -s "http://127.0.0.1:8791/api/v1/services/gbk-decoded/logs?tail=8" | jq -r '.lines[].text'
```
预期:`启动完成:温度传感器已连接` / `采集数据:温度 36.5C 湿度 65%` / `警告:数据写入 lux 数据库` 全部正确。

### 5. (可选)SSE 实时日志流
```bash
curl -N "http://127.0.0.1:8791/api/v1/services/gbk-decoded/logs/stream"
```
预期:每 0.5s 滚出一行正确中文;`Ctrl-C` 退出 curl(不影响 warden)。

### 6. 优雅停止(看 warden 终端日志)
```bash
curl -X POST http://127.0.0.1:8791/api/v1/services/gbk-decoded/stop
curl -X POST http://127.0.0.1:8791/api/v1/services/gbk-raw/stop
```
在 warden 终端预期:`发送优雅停止信号` →(helper 不响应信号,2s 后)`优雅停止超时,强杀进程树` → `已停止`。

### 7. 清理
在 warden 终端按 `Ctrl-C`(触发 stop_all graceful 退出),然后确认无残留:
```bash
procs gbk_target   # 应无输出行
```

## 额外可选测试

- **编码别名**:把 `config/services.test.toml` 里 `gbk-decoded` 的 `output_encoding = "gbk"` 改成 `"cp936"` 或 `"gb2312"`,重启 warden,效果应一致(别名都解析到 GBK)。
- **未知编码回退**:改成 `output_encoding = "xxx"`,重启 → warden 终端打印 `[supervisor] 未知输出编码 'xxx',回退 UTF-8`,中文回到乱码(证明降级安全)。
