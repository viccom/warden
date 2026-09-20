# warden CLI 发行包 —— 快速开始

warden 是一个进程监护工具(supervisord / pm2 风格):把一组本地程序交给它拉起、监护、
看日志、增删改,并提供一个 HTTP API + Web UI。**本包是 CLI 单文件版**,解压即用,
不需要任何运行时依赖。

包内最权威的操作文档是 [`AGENT-GUIDE.md`](./AGENT-GUIDE.md)(含完整 API、配置字段、
排障表与自动化配方);本文件只讲三分钟上手。

## 包内容

| 路径 | 说明 |
|---|---|
| `warden`(`warden.exe`) | 主程序(监护引擎 + HTTP API + TUI + Web UI 都在里面) |
| `config/services.toml` | 开箱即用的配置(两个演示服务,自动启动) |
| `config/services.example.toml` | 全字段参考配置,逐字段中文注释 —— 正式部署照它改 |
| `AGENT-GUIDE.md` | 完整使用手册(AI Agent / 自动化友好) |
| `LICENSE` | MIT |

## 1. 启动

**Linux**

```bash
chmod +x ./warden
./warden run
```

**Windows**(cmd / PowerShell,在包根目录)

```powershell
.\warden.exe run
```

`config/services.toml` 放在可执行文件同级的 `config/` 目录下会被自动发现,所以不用传 `--config`。
首次启动会在包根目录创建 `data/`(被监护进程日志)与 `logs/`(warden 自身日志)。

## 2. 验证

另开一个终端:

```bash
curl http://127.0.0.1:8789/api/v1/health
# {"failed":0,"running":1,"services":1,"status":"ok","title":null,"version":"0.3.0"}
```

浏览器打开 <http://127.0.0.1:8789/> 就是 Web UI(看状态、看日志、改配置)。

## 3. 换成你自己的服务

编辑 `config/services.toml`,把演示服务替换成真实程序;字段含义照抄
`config/services.example.toml` 的注释。改完不用重启 daemon:

```bash
curl -X POST http://127.0.0.1:8789/api/v1/config/reload      # 热重载配置
```

也可以用 HTTP API 增删改(会写回同一个配置文件,保留注释):

```bash
curl -X POST http://127.0.0.1:8789/api/v1/services -H 'Content-Type: application/json' \
  -d '{"name":"my-app","command":"bin/app","args":["--port","9000"],"auto_start":true,"auto_restart":true}'
curl -X POST http://127.0.0.1:8789/api/v1/services/my-app/start
```

## 4. 停止 / 常驻

- 前台运行时按 `Ctrl-C` 即可(触发优雅停机:先停所有被监护服务,再关 HTTP)。
- 想让 warden 开机自启:
  - Windows:`.\warden.exe install`(自动请求管理员提权,注册服务名 `warden`),`sc start warden`
  - Linux:`sudo ./warden install`(写 `/etc/systemd/system/warden.service`),`sudo systemctl enable --now warden`

## 安全提醒

默认只监听 `127.0.0.1`(仅本机可访问),此时 `auth_token` 留空是安全的。
一旦把 `api_bind` 改成对外地址,**必须**设置 `auth_token` —— 能访问 API 就等于能在本机启停任意进程。

有问题先查 `logs/warden.log.<日期>`;常见故障(端口占用、中文乱码、服务反复重启等)对照
`AGENT-GUIDE.md` §6 排障表。
