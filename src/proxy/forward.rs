//! 转发层:头处理 + 流式直传 + WebSocket 隧道。
//!
//! 设计见 PLAN-REVERSE-PROXY.md §4.4。本模块按 TDD 分三步:
//! Task 8 头处理(纯函数)→ Task 9 流式直传/错误页 → Task 11 WebSocket 隧道。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri};
use axum::response::IntoResponse;

use crate::proxy::router::Decision;
use crate::proxy::ProxyState;

/// hop-by-hop 头(RFC 7230 §6.1 + 常见代理专用头)。
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade", // WebSocket 升级路径单独处理(Task 11),非升级请求剥掉
];

/// 重写请求头:剥离 hop-by-hop + Connection 令牌、重写 Host、追加 X-Forwarded-*。
///
/// - `client_ip`:客户端 IP(从连接信息取)
/// - `upstream_authority`:上游 host:port(从 to URL 解析)
/// - `scheme`:"http" | "https"(TLS 终止后,P2 起区分)
/// - `preserve_host`:true 时保留原 Host,false 时设为 upstream_authority
pub fn rewrite_headers(
    headers: &mut HeaderMap,
    client_ip: &str,
    upstream_authority: &str,
    scheme: &str,
    preserve_host: bool,
) {
    // 原始 Host 在改写前捕获(X-Forwarded-Host 语义 = 客户端请求的 host)
    let original_host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // 1. Connection 头:其令牌列表列出的头也是逐跳的,连同自身一起剥
    let mut condemned: Vec<HeaderName> = Vec::new();
    if let Some(conn) = headers.get("connection").and_then(|v| v.to_str().ok()) {
        for token in conn.split(',') {
            if let Ok(name) = HeaderName::from_bytes(token.trim().as_bytes()) {
                condemned.push(name);
            }
        }
    }
    headers.remove("connection");
    for name in condemned {
        headers.remove(&name);
    }
    // 2. 其余 hop-by-hop
    for h in HOP_BY_HOP {
        headers.remove(*h);
    }
    // 3. Host:缺省重写为上游 authority;preserve_host 保留原值
    if !preserve_host {
        if let Ok(v) = HeaderValue::from_str(upstream_authority) {
            headers.insert("host", v);
        }
    }
    // 4. X-Forwarded-For:已有以 ", " 续接客户端 IP(链式代理语义),无则插入
    let xff = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(existing) => format!("{existing}, {client_ip}"),
        None => client_ip.to_owned(),
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        headers.insert("x-forwarded-for", v);
    }
    // 5. X-Forwarded-Proto / X-Forwarded-Host
    // XFP:前置代理(TLS 终止方)已注入则透传(链式语义);无才注入自身 scheme
    if headers.get("x-forwarded-proto").is_none() {
        if let Ok(v) = HeaderValue::from_str(scheme) {
            headers.insert("x-forwarded-proto", v);
        }
    }
    if let Some(orig) = original_host {
        if let Ok(v) = HeaderValue::from_str(&orig) {
            headers.insert("x-forwarded-host", v);
        }
    }
}

// ── 转发 handler(Task 9:流式直传 + 421/502/503 错误页)────────────────

/// 服务引用解析失败的原因(映射 404/503/502,设计 §4.3)。
enum ServiceUpstreamError {
    NotFound,
    Stopped(String),
    NoUiUrl,
}

/// 服务引用/auto 路由的上游解析:服务须存在、Running、ui_url 已配置。
async fn service_upstream(state: &ProxyState, name: &str) -> Result<String, ServiceUpstreamError> {
    let st = state
        .router
        .supervisor()
        .status(name)
        .map_err(|_| ServiceUpstreamError::NotFound)?;
    if !st.state.is_running() {
        return Err(ServiceUpstreamError::Stopped(st.state.name().to_owned()));
    }
    match st.ui_url.as_deref() {
        Some(u) if !u.is_empty() => Ok(u.to_owned()),
        _ => Err(ServiceUpstreamError::NoUiUrl),
    }
}

/// 代理主 handler:按 Host 路由决策分支转发,流式透传(零缓冲)。
pub async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    req: Request<axum::body::Body>,
) -> Response<axum::body::Body> {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let log = ReqLog {
        method: req.method().clone(),
        host: host.clone(),
        path: req.uri().path().to_owned(),
        started: std::time::Instant::now(),
    };
    if host.is_empty() {
        log.emit(StatusCode::BAD_REQUEST, "-");
        return error_page(StatusCode::BAD_REQUEST, &host, "请求缺少 Host 头");
    }
    let decision = state.router.resolve(&host);
    let (upstream, preserve_host, svc) = match decision {
        Decision::Route { to, preserve_host } => (to, preserve_host, None),
        Decision::RouteService {
            service,
            preserve_host,
        } => (String::new(), preserve_host, Some(service)),
        Decision::AutoService { name } => (
            String::new(),
            state.router.global_preserve_host(),
            Some(name),
        ),
        Decision::NotFound => {
            log.emit(StatusCode::MISDIRECTED_REQUEST, "-");
            return error_page(
                StatusCode::MISDIRECTED_REQUEST,
                &host,
                "未知 Host(无精确/通配路由命中,auto 路由亦无此服务)",
            );
        }
    };
    let upstream = match svc {
        // 服务引用(显式 service / auto):运行时经 snapshot 解析 ui_url,
        // 服务状态即路由可用性(停止 503、缺 ui_url 502、不存在 404)
        Some(name) => match service_upstream(&state, &name).await {
            Ok(u) => u,
            Err(ServiceUpstreamError::NotFound) => {
                log.emit(StatusCode::NOT_FOUND, &format!("service:{name}"));
                return error_page(
                    StatusCode::NOT_FOUND,
                    &host,
                    &format!("服务 '{name}' 不存在"),
                );
            }
            Err(ServiceUpstreamError::Stopped(state_name)) => {
                log.emit(StatusCode::SERVICE_UNAVAILABLE, &format!("service:{name}"));
                return error_page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &host,
                    &format!("服务 '{name}' 当前状态 {state_name}(未运行),稍后重试"),
                );
            }
            Err(ServiceUpstreamError::NoUiUrl) => {
                log.emit(StatusCode::BAD_GATEWAY, &format!("service:{name}"));
                return error_page(
                    StatusCode::BAD_GATEWAY,
                    &host,
                    &format!("服务 '{name}' 未配置 ui_url,无法转发"),
                );
            }
        },
        None => upstream,
    };
    // WebSocket 升级请求走专用隧道(不经通用转发:握手头须透传,双向复制)
    if is_websocket_upgrade(req.headers()) {
        return websocket_tunnel(&state, req, &client, &upstream, preserve_host, &log).await;
    }
    forward_to(&state, req, &client, &upstream, preserve_host, &log).await
}

/// http → https 301 入口(80/443 同配时 http listener 全量重定向)。
/// Host 经规范化+字符白名单校验后进 Location(防 Host 注入);入站端口剥离,
/// 目标端口取 https 配置(443 不附加)。
pub async fn redirect_to_https(
    State(port): State<u16>,
    req: Request<axum::body::Body>,
) -> Response<axum::body::Body> {
    let host_hdr = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let host = crate::proxy::router::HostRouter::normalize_host(&host_hdr);
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return error_page(StatusCode::BAD_REQUEST, &host_hdr, "请求 Host 头非法");
    }
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let authority = if port == 443 {
        host
    } else {
        format!("{host}:{port}")
    };
    let location = format!("https://{authority}{pq}");
    match Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("location", &location)
        .body(axum::body::Body::empty())
    {
        Ok(resp) => resp,
        Err(e) => error_page(
            StatusCode::BAD_REQUEST,
            &host_hdr,
            &format!("构造重定向失败:{e}"),
        ),
    }
}

/// WebSocket 升级检测:Upgrade 头含 websocket 令牌(RFC 7230 允许逗号分隔
/// 列多协议,逐 token 匹配)且 Connection 含 upgrade 令牌(RFC 6455 握手形态)。
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let up = headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("websocket"))
        });
    let conn = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    up && conn
}

/// WebSocket 隧道(设计 §4.4):上游 101 后取两侧升级流,
/// `copy_bidirectional` 双向复制;隧道无超时,shutdown 时随 drain 上限强关。
async fn websocket_tunnel(
    state: &ProxyState,
    req: Request<axum::body::Body>,
    client: &SocketAddr,
    upstream: &str,
    preserve_host: bool,
    log: &ReqLog,
) -> Response<axum::body::Body> {
    use hyper::upgrade::OnUpgrade;
    use hyper_util::rt::TokioIo;

    let uri = match build_upstream_uri(upstream, req.uri().path_and_query()) {
        Ok(u) => u,
        Err(e) => {
            let detail = format!("上游 URI 非法({upstream}):{e}");
            log.emit(StatusCode::BAD_GATEWAY, upstream);
            return error_page(StatusCode::BAD_GATEWAY, &log.host, &detail);
        }
    };
    let Some(authority) = uri.authority().map(|a| a.as_str().to_owned()) else {
        let detail = format!("上游 URI 缺少 authority:{upstream}");
        log.emit(StatusCode::BAD_GATEWAY, upstream);
        return error_page(StatusCode::BAD_GATEWAY, &log.host, &detail);
    };
    let (mut parts, body) = req.into_parts();
    // 客户端侧升级句柄:hyper server 塞在握手请求的 extensions 里
    let on_client = parts.extensions.remove::<OnUpgrade>();
    // 握手语义须透传上游:常规重写(剥全部 hop-by-hop)后补回升级头
    let saved_upgrade = parts.headers.get("upgrade").cloned();
    rewrite_headers(
        &mut parts.headers,
        &client.ip().to_string(),
        &authority,
        state.scheme,
        preserve_host,
    );
    if let Some(up) = saved_upgrade {
        parts.headers.insert("upgrade", up);
        parts
            .headers
            .insert("connection", HeaderValue::from_static("Upgrade"));
    }
    let mut builder = Request::builder().method(parts.method.clone()).uri(uri);
    for (k, v) in &parts.headers {
        builder = builder.header(k, v);
    }
    let upstream_req = match builder.body(body) {
        Ok(r) => r,
        Err(e) => {
            let detail = format!("构造上游升级请求失败:{e}");
            log.emit(StatusCode::BAD_GATEWAY, upstream);
            return error_page(StatusCode::BAD_GATEWAY, &log.host, &detail);
        }
    };
    // hyper client 收到 101 时连接退出连接池,升级句柄在 response extensions
    match state.client.request(upstream_req).await {
        Ok(mut resp) if resp.status() == StatusCode::SWITCHING_PROTOCOLS => {
            let on_upstream = resp.extensions_mut().remove::<OnUpgrade>();
            let status = resp.status();
            let (up_parts, _) = resp.into_parts();
            let mut builder = Response::builder().status(status);
            for (k, v) in up_parts.headers.iter() {
                // 101 响应的 connection/upgrade 保留(客户端握手依赖),其余照剥
                let n = k.as_str();
                let keep =
                    n.eq_ignore_ascii_case("connection") || n.eq_ignore_ascii_case("upgrade");
                if !keep && is_hop_by_hop(n) {
                    continue;
                }
                builder = builder.header(k, v);
            }
            log.emit(status, upstream);
            let resp = match builder.body(axum::body::Body::empty()) {
                Ok(r) => r,
                Err(e) => {
                    return error_page(
                        StatusCode::BAD_GATEWAY,
                        &log.host,
                        &format!("构造 101 响应失败:{e}"),
                    )
                }
            };
            match (on_client, on_upstream) {
                (Some(c), Some(u)) => {
                    tokio::spawn(async move {
                        // 两侧升级流在 101 响应返回后 ready;TokioIo 把
                        // hyper 的 Read/Write 适配为 tokio AsyncRead/Write
                        match tokio::join!(c, u) {
                            (Ok(cl), Ok(up)) => {
                                let mut cl = TokioIo::new(cl);
                                let mut up = TokioIo::new(up);
                                if let Err(e) =
                                    tokio::io::copy_bidirectional(&mut cl, &mut up).await
                                {
                                    tracing::debug!("[proxy] ws 隧道关闭:{e}");
                                }
                            }
                            _ => tracing::debug!("[proxy] ws 隧道升级失败(一侧未就绪)"),
                        }
                    });
                }
                _ => {
                    tracing::warn!("[proxy] ws 101 但缺少升级句柄,隧道未建立(client 侧/上游侧)")
                }
            }
            resp
        }
        // 上游拒绝升级(非 101):按普通响应透传(走通用头过滤)
        Ok(resp) => {
            let status = resp.status();
            let (up_parts, up_body) = resp.into_parts();
            let mut builder = Response::builder().status(status);
            for (k, v) in up_parts.headers.iter() {
                if is_hop_by_hop(k.as_str()) {
                    continue;
                }
                builder = builder.header(k, v);
            }
            log.emit(status, upstream);
            match builder.body(axum::body::Body::new(up_body)) {
                Ok(r) => r,
                Err(e) => error_page(
                    StatusCode::BAD_GATEWAY,
                    &log.host,
                    &format!("构造响应失败:{e}"),
                ),
            }
        }
        Err(e) => {
            let detail = format!("上游连接失败({upstream}):{}", error_chain(&e));
            log.emit(StatusCode::BAD_GATEWAY, upstream);
            error_page(StatusCode::BAD_GATEWAY, &log.host, &detail)
        }
    }
}

/// 解析并转发到上游 URI(to 可带 path 前缀,与请求 path 拼接)。
async fn forward_to(
    state: &ProxyState,
    req: Request<axum::body::Body>,
    client: &SocketAddr,
    upstream: &str,
    preserve_host: bool,
    log: &ReqLog,
) -> Response<axum::body::Body> {
    let uri = match build_upstream_uri(upstream, req.uri().path_and_query()) {
        Ok(u) => u,
        Err(e) => {
            let detail = format!("上游 URI 非法({upstream}):{e}");
            log.emit(StatusCode::BAD_GATEWAY, upstream);
            return error_page(StatusCode::BAD_GATEWAY, &log.host, &detail);
        }
    };
    let Some(authority) = uri.authority().map(|a| a.as_str().to_owned()) else {
        let detail = format!("上游 URI 缺少 authority:{upstream}");
        log.emit(StatusCode::BAD_GATEWAY, upstream);
        return error_page(StatusCode::BAD_GATEWAY, &log.host, &detail);
    };
    let (mut parts, body) = req.into_parts();
    rewrite_headers(
        &mut parts.headers,
        &client.ip().to_string(),
        &authority,
        state.scheme,
        preserve_host,
    );
    let mut builder = Request::builder().method(parts.method.clone()).uri(uri);
    for (k, v) in &parts.headers {
        builder = builder.header(k, v);
    }
    let upstream_req = match builder.body(body) {
        Ok(r) => r,
        Err(e) => {
            let detail = format!("构造上游请求失败:{e}");
            log.emit(StatusCode::BAD_GATEWAY, upstream);
            return error_page(StatusCode::BAD_GATEWAY, &log.host, &detail);
        }
    };
    // 流式直传:请求/响应 body 全程透传(SSE/大文件零缓冲);
    // 不设总时长超时(会误杀 SSE),连接建立超时由 connector 承担
    match state.client.request(upstream_req).await {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            let (up_parts, up_body) = upstream_resp.into_parts();
            let mut builder = Response::builder().status(status);
            for (k, v) in up_parts.headers.iter() {
                // 响应侧剥 hop-by-hop;CL/TE 不手抄(hyper 按流自动重组帧,避免双 framing)
                if is_hop_by_hop(k.as_str()) {
                    continue;
                }
                builder = builder.header(k, v);
            }
            log.emit(status, upstream);
            match builder.body(axum::body::Body::new(up_body)) {
                Ok(r) => r,
                Err(e) => error_page(
                    StatusCode::BAD_GATEWAY,
                    &log.host,
                    &format!("构造响应失败:{e}"),
                ),
            }
        }
        Err(e) => {
            let detail = format!("上游连接失败({upstream}):{}", error_chain(&e));
            log.emit(StatusCode::BAD_GATEWAY, upstream);
            error_page(StatusCode::BAD_GATEWAY, &log.host, &detail)
        }
    }
}

/// 上游 URI 构造:`to`(http://host[:port][/prefix])+ 请求 path-and-query 拼接。
fn build_upstream_uri(
    upstream: &str,
    path_and_query: Option<&axum::http::uri::PathAndQuery>,
) -> Result<Uri, String> {
    let base: Uri = upstream.parse().map_err(|e| format!("{e}"))?;
    let scheme = base.scheme_str().unwrap_or("http");
    let authority = base
        .authority()
        .ok_or_else(|| "缺少 authority".to_owned())?
        .as_str();
    let prefix = base.path().trim_end_matches('/');
    let pq = path_and_query.map(|p| p.as_str()).unwrap_or("/");
    let full = if prefix.is_empty() {
        format!("{scheme}://{authority}{pq}")
    } else {
        format!("{scheme}://{authority}{prefix}{pq}")
    };
    full.parse().map_err(|e| format!("{e}"))
}

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding")
}

/// 错误链展开(上游连接失败时 hyper-util 顶层 Display 极浅,如
/// "client error (Connect)";真实原因——DNS/超时/TLS 验证——在 source 里)。
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(&format!(": {c}"));
        cur = c.source();
    }
    s
}

/// HTML 实体转义(错误页插值点防注入:host/detail 来自客户端输入)。
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// access log 单行(设计 §4.4):method/host/path/status/latency/upstream。
/// path 只传路径部分(调用方用 uri().path(),不含 query——敏感查询串不落日志)。
fn access_line(
    method: &str,
    host: &str,
    path: &str,
    status: u16,
    latency_ms: u128,
    upstream: &str,
) -> String {
    format!("[proxy] {method} {host}{path} {status} {latency_ms}ms upstream={upstream}")
}

/// 请求级日志上下文(handler 各返回分支共用)。
struct ReqLog {
    method: axum::http::Method,
    host: String,
    path: String,
    started: std::time::Instant,
}

impl ReqLog {
    fn emit(&self, status: StatusCode, upstream: &str) {
        tracing::info!(
            "{}",
            access_line(
                self.method.as_str(),
                &self.host,
                &self.path,
                status.as_u16(),
                self.started.elapsed().as_millis(),
                upstream
            )
        );
    }
}

/// 极简错误页(HTML):含状态、Host、原因(设计 §4.5,不进 WardenError)。
/// 插值全部经 html_escape(host/detail 含客户端输入)。
fn error_page(status: StatusCode, host: &str, detail: &str) -> Response<axum::body::Body> {
    let body = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{}</title></head>\
         <body><h1>{}</h1><p>Host: {}</p><p>{}</p><hr><p>warden reverse proxy</p></body></html>",
        status.as_str(),
        status,
        html_escape(host),
        html_escape(detail)
    );
    (status, [("content-type", "text/html; charset=utf-8")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("connection", "keep-alive, X-Custom".parse().unwrap());
        h.insert("x-custom", "v".parse().unwrap());
        h.insert("host", "a.example.com".parse().unwrap());
        h.insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
        h
    }

    #[test]
    fn strips_hop_by_hop() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert!(h.get("connection").is_none(), "Connection 被剥");
        assert!(h.get("keep-alive").is_none());
    }

    #[test]
    fn strips_connection_tokens() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert!(h.get("x-custom").is_none(), "Connection 令牌列出的头也要剥");
    }

    #[test]
    fn rewrites_host_by_default() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert_eq!(h.get("host").unwrap(), "upstream:8080");
    }

    #[test]
    fn preserves_host_when_configured() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", true);
        assert_eq!(h.get("host").unwrap(), "a.example.com");
    }

    #[test]
    fn appends_xff_with_comma() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        let xff = h.get("x-forwarded-for").unwrap().to_str().unwrap();
        assert_eq!(xff, "10.0.0.1, 203.0.113.5", "已有 XFF 以逗号续接");
    }

    #[test]
    fn adds_xff_when_absent() {
        let mut h = HeaderMap::new();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "http", false);
        assert_eq!(h.get("x-forwarded-for").unwrap(), "203.0.113.5");
    }

    #[test]
    fn sets_xfp_and_xfh() {
        let mut h = build();
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "https", false);
        assert_eq!(h.get("x-forwarded-proto").unwrap(), "https");
        assert_eq!(h.get("x-forwarded-host").unwrap(), "a.example.com");
    }

    /// 意图:前置代理(如 OpenResty TLS 终止)已注入 X-Forwarded-Proto 时,
    /// warden 作为链上后置代理必须透传该值,不得用自身 scheme(明文 http)覆盖——
    /// 否则上游 OAuth 回调/Secure Cookie/CSP 拿到错误的客户端协议。
    #[test]
    fn preserves_existing_xfp_from_front_proxy() {
        let mut h = build();
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        rewrite_headers(&mut h, "203.0.113.5", "upstream:8080", "http", false);
        assert_eq!(
            h.get("x-forwarded-proto").unwrap(),
            "https",
            "已有 XFP(前置 TLS 终止)应透传,不被自身 scheme 覆盖"
        );
        // 无 XFP 时行为不变:注入自身 scheme
        let mut h2 = build();
        rewrite_headers(&mut h2, "203.0.113.5", "upstream:8080", "http", false);
        assert_eq!(h2.get("x-forwarded-proto").unwrap(), "http");
    }

    /// 意图:RFC 7230 允许 Upgrade 头逗号分隔列多协议,websocket 不必是
    /// 唯一值——全值相等比较会把合规握手误判为非 WS(剥头后上游 400)。
    #[test]
    fn websocket_upgrade_detects_multi_protocol_header() {
        let mut h = HeaderMap::new();
        h.insert("upgrade", "WebSocket, h2c".parse().unwrap());
        h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        assert!(
            is_websocket_upgrade(&h),
            "多协议 Upgrade 头应识别出 websocket"
        );
    }

    /// 意图:非 websocket 升级(h2c)与缺 Connection 令牌的请求都不得进隧道。
    #[test]
    fn websocket_upgrade_rejects_non_ws_forms() {
        let mut h = HeaderMap::new();
        h.insert("upgrade", "h2c".parse().unwrap());
        h.insert("connection", "Upgrade".parse().unwrap());
        assert!(!is_websocket_upgrade(&h), "非 ws 协议不进隧道");

        let mut h = HeaderMap::new();
        h.insert("upgrade", "websocket".parse().unwrap());
        h.insert("connection", "keep-alive".parse().unwrap());
        assert!(!is_websocket_upgrade(&h), "无 upgrade 令牌不进隧道");
    }

    /// 意图:错误页插值(host/detail 来自客户端输入)必须 HTML 转义,
    /// 防 Host 头注入脚本(对齐 Web UI textContent 防 XSS 的既有防线)。
    #[test]
    fn html_escape_neutralizes_markup() {
        let e = html_escape(r#"<script>alert("x&y")</script>"#);
        assert!(!e.contains('<'), "不应残留原始 <: {e}");
        assert!(!e.contains('>'), "不应残留原始 >: {e}");
        assert!(e.contains("&lt;script&gt;"), "标签实体化: {e}");
        assert!(e.contains("&amp;"), "and 符号实体化: {e}");
        assert!(e.contains("&quot;"), "引号实体化: {e}");
    }

    /// 意图:access log 行格式——method/host/path/status/latency/upstream,
    /// path 只传路径部分(调用方用 uri().path(),天然不含 query,防敏感串落日志)。
    #[test]
    fn access_line_format() {
        let line = access_line(
            "GET",
            "fs.x.com",
            "/api/v1/img",
            200,
            42,
            "http://127.0.0.1:8790",
        );
        assert_eq!(
            line,
            "[proxy] GET fs.x.com/api/v1/img 200 42ms upstream=http://127.0.0.1:8790"
        );
    }
}
