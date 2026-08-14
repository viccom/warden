//! TUI 的 HTTP API 客户端:连 warden daemon 的 `/api/v1/*` 端点。
//!
//! 轻量反序列化(不耦合后端 DTO):`state` 用 `serde_json::Value` 解析
//! (后端 ProcState 是 `tag = "state"` 形态,取 `.state`/`.pid`/`.reason` 渲染)。
//! SSE 日志流用 reqwest-eventsource(自带断线重连)。

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// 服务状态行(从 `GET /services` 的 `{services:[...]}` 反序列化)。
#[derive(Clone, Deserialize, Debug)]
pub struct ServiceView {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    /// ProcState 序列化形态:`{"state":"running","pid":...}` 等。
    pub state: Value,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub metrics: MetricsView,
    #[serde(default)]
    pub health: ServiceHealthView,
    #[serde(default)]
    pub last_exit: Option<LastExitView>,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub auto_restart: bool,
    /// 环境变量(daemon 全局已烘入;详情面板展示)。
    #[serde(default)]
    pub environment: std::collections::HashMap<String, String>,
}

/// 健康检查结果(backend HealthStatus)。
#[derive(Clone, Deserialize, Debug, Default)]
pub struct ServiceHealthView {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
}

/// 最近一次自然退出。
#[derive(Clone, Deserialize, Debug)]
pub struct LastExitView {
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub at: String,
}

impl ServiceView {
    /// 状态单字名(running/stopped/failed/restarting/starting/stopping/unknown)。
    pub fn state_name(&self) -> &str {
        self.state
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    }

    pub fn pid(&self) -> Option<u32> {
        self.state
            .get("pid")
            .and_then(Value::as_u64)
            .map(|p| p as u32)
    }

    pub fn state_reason(&self) -> Option<String> {
        self.state
            .get("reason")
            .and_then(Value::as_str)
            .map(String::from)
    }

    /// 展示名:display_name 空则回退 name。
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() {
            &self.name
        } else {
            &self.display_name
        }
    }
}

/// 资源采样(字段缺省时 default)。
#[derive(Clone, Deserialize, Debug, Default)]
pub struct MetricsView {
    #[serde(default)]
    pub cpu_percent: f32,
    #[serde(default)]
    pub memory_kb: u64,
}

/// 单行日志(stream: stdout/stderr;level: 启发式等级;ts: ISO 时间;text)。
#[derive(Clone, Deserialize, Debug)]
pub struct LogLineView {
    #[serde(default)]
    pub stream: String,
    /// 启发式识别的等级:debug/info/warn/error/unknown。
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Deserialize)]
struct ServicesResponse {
    services: Vec<ServiceView>,
}

#[derive(Deserialize)]
struct LogsResponse {
    lines: Vec<LogLineView>,
}

#[derive(Deserialize)]
pub struct HealthView {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub running: u32,
    #[serde(default)]
    pub failed: u32,
}

/// warden HTTP API 客户端。
#[derive(Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl ApiClient {
    pub fn new(base: String, token: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.get(format!("{}{}", self.base, path));
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.post(format!("{}{}", self.base, path));
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// 服务列表(名称/状态/PID/metrics 等)。
    pub async fn list(&self) -> anyhow::Result<Vec<ServiceView>> {
        let resp: ServicesResponse = self
            .get("/api/v1/services")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.services)
    }

    /// 生命周期操作:start / stop / restart。
    pub async fn action(&self, name: &str, act: &str) -> anyhow::Result<()> {
        let url = format!("/api/v1/services/{}/{}", urlencode(name), act);
        self.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    /// 全启(start-all 端点,注意连字符)。
    pub async fn start_all(&self) -> anyhow::Result<()> {
        self.post("/api/v1/services/start-all")
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// 全停(stop-all 端点)。
    pub async fn stop_all(&self) -> anyhow::Result<()> {
        self.post("/api/v1/services/stop-all")
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// 日志快照(最近 n 行)。
    pub async fn logs_tail(&self, name: &str, n: usize) -> anyhow::Result<Vec<LogLineView>> {
        let url = format!("/api/v1/services/{}/logs?tail={}", urlencode(name), n);
        let resp: LogsResponse = self
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.lines)
    }

    /// health(取版本号)。
    pub async fn health(&self) -> anyhow::Result<HealthView> {
        Ok(self
            .get("/api/v1/health")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// SSE 实时日志流(带 token 时仍可工作:eventsource 支持自定义 RequestBuilder)。
    pub fn logs_stream(&self, name: &str) -> reqwest_eventsource::EventSource {
        let url = format!(
            "{}/api/v1/services/{}/logs/stream",
            self.base,
            urlencode(name)
        );
        let req = self.client.get(url);
        let req = match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        };
        reqwest_eventsource::EventSource::new(req).expect("request 可 clone")
    }
}

/// 服务名 URL path 编码(空格等)。
fn urlencode(name: &str) -> String {
    name.replace(' ', "%20")
}
