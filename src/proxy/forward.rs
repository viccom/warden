//! 转发层:头处理 + 流式直传 + WebSocket 隧道。
//!
//! 设计见 PLAN-REVERSE-PROXY.md §4.4。本模块按 TDD 分三步:
//! Task 8 头处理(纯函数)→ Task 9 流式直传/错误页 → Task 11 WebSocket 隧道。

use axum::http::{HeaderMap, HeaderName, HeaderValue};

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
