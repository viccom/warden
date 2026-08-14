//! 日志端点:快照 + SSE 实时流。

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::api::AppState;
use crate::error::WResult;
use crate::logs::LogLine;

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_tail")]
    pub tail: usize,
}

fn default_tail() -> usize {
    500
}

/// 日志快照(最近 N 行)。
pub async fn logs(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<LogsQuery>,
) -> WResult<impl IntoResponse> {
    let hub = st.supervisor.log_hub(&name)?;
    let lines = hub.snapshot(q.tail);
    Ok(Json(json!({ "name": name, "tail": q.tail, "lines": lines })))
}

/// SSE 实时日志流(订阅 LogHub 的 broadcast)。
pub async fn logs_stream(
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> WResult<Response> {
    let hub = st.supervisor.log_hub(&name)?;
    let rx = hub.subscribe();
    let stream = BroadcastStream::new(rx)
        .filter_map(|r| async move { r.ok() })
        .map(|line: LogLine| {
            Ok::<_, std::convert::Infallible>(
                Event::default()
                    .event("log")
                    .data(serde_json::to_string(&line).unwrap_or_default()),
            )
        });
    Ok(Sse::new(stream).into_response())
}
