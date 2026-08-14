//! daemon 健康端点(白名单,无需鉴权)。

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::api::AppState;

pub async fn health(State(st): State<AppState>) -> impl IntoResponse {
    let list = st.supervisor.list();
    let total = list.len();
    let running = list.iter().filter(|s| s.state.is_running()).count();
    let failed = list
        .iter()
        .filter(|s| s.state.name() == "failed")
        .count();
    Json(json!({
        "status": "ok",
        "version": st.version,
        "services": total,
        "running": running,
        "failed": failed,
    }))
}
