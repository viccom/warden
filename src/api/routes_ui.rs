//! Web UI:返回嵌入的单页面(连本地 HTTP API)。
//!
//! 用 `include_str!` 把 `web/index.html` 编译进二进制,零额外依赖、单文件部署。
//! 页面经浏览器 fetch / EventSource 调用同源 `/api/v1/*` 端点。
//! Phase 4 完整 Web 再换 rust-embed 多资源嵌入。

use axum::response::Html;

/// 返回 UI 页面(`Html` 保证 Content-Type: text/html,浏览器渲染而非当纯文本)。
pub async fn index() -> Html<&'static str> {
    Html(include_str!("../../web/index.html"))
}
