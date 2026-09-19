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
    if let Ok(v) = HeaderValue::from_str(scheme) {
        headers.insert("x-forwarded-proto", v);
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
        .unwrap_or_default();
    if host.is_empty() {
        return error_page(StatusCode::BAD_REQUEST, host, "请求缺少 Host 头");
    }
    let decision = state.router.resolve(host);
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
            return error_page(
                StatusCode::MISDIRECTED_REQUEST,
                host,
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
                return error_page(
                    StatusCode::NOT_FOUND,
                    host,
                    &format!("服务 '{name}' 不存在"),
                );
            }
            Err(ServiceUpstreamError::Stopped(state_name)) => {
                return error_page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    host,
                    &format!("服务 '{name}' 当前状态 {state_name}(未运行),稍后重试"),
                );
            }
            Err(ServiceUpstreamError::NoUiUrl) => {
                return error_page(
                    StatusCode::BAD_GATEWAY,
                    host,
                    &format!("服务 '{name}' 未配置 ui_url,无法转发"),
                );
            }
        },
        None => upstream,
    };
    forward_to(&state, req, &client, &upstream, preserve_host).await
}

/// 解析并转发到上游 URI(to 可带 path 前缀,与请求 path 拼接)。
async fn forward_to(
    state: &ProxyState,
    req: Request<axum::body::Body>,
    client: &SocketAddr,
    upstream: &str,
    preserve_host: bool,
) -> Response<axum::body::Body> {
    let uri = match build_upstream_uri(upstream, req.uri().path_and_query()) {
        Ok(u) => u,
        Err(e) => {
            return error_page(
                StatusCode::BAD_GATEWAY,
                "",
                &format!("上游 URI 非法({upstream}):{e}"),
            )
        }
    };
    let Some(authority) = uri.authority().map(|a| a.as_str().to_owned()) else {
        return error_page(
            StatusCode::BAD_GATEWAY,
            "",
            &format!("上游 URI 缺少 authority:{upstream}"),
        );
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
            return error_page(
                StatusCode::BAD_GATEWAY,
                "",
                &format!("构造上游请求失败:{e}"),
            )
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
            match builder.body(axum::body::Body::new(up_body)) {
                Ok(r) => r,
                Err(e) => error_page(StatusCode::BAD_GATEWAY, "", &format!("构造响应失败:{e}")),
            }
        }
        Err(e) => error_page(
            StatusCode::BAD_GATEWAY,
            "",
            &format!("上游连接失败({upstream}):{e}"),
        ),
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

/// 极简错误页(HTML):含状态、Host、原因(设计 §4.5,不进 WardenError)。
fn error_page(status: StatusCode, host: &str, detail: &str) -> Response<axum::body::Body> {
    let body = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{}</title></head>\
         <body><h1>{}</h1><p>Host: {}</p><p>{}</p><hr><p>warden reverse proxy</p></body></html>",
        status.as_str(),
        status,
        host,
        detail
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
}
