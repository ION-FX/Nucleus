//! Scripting API exposed under `/api/v1` and authenticated with API keys
//! (`Authorization: Bearer nuc_xxxx`). Every route reuses the same permission
//! model as the web UI.

use crate::auth::user_for_api_key;
use crate::models::ServerRow;
use crate::perms;
use crate::routes::*;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use serde_json::json;
use std::sync::Arc;

fn api_user(app: &App, headers: &HeaderMap) -> Option<User> {
    user_for_api_key(&app.db, headers)
}

/// List servers the caller can see, with a quick daemon status for each.
pub async fn api_list_servers(State(app): State<SharedApp>, headers: HeaderMap) -> Response {
    let Some(u) = api_user(&app, &headers) else {
        return json_err(StatusCode::UNAUTHORIZED, "invalid api key");
    };
    let servers = list_servers(&app);
    let mut out = Vec::new();
    for s in servers {
        if !perms::has_any_access(&app.db, &u, &s) {
            continue;
        }
        let node_name = get_node(&app, &s.node_id).map(|n| n.name).unwrap_or_default();
        let status = match get_node(&app, &s.node_id) {
            Some(n) => crate::daemon::DaemonClient::new(&app, &n)
                .status(&s.id)
                .await
                .ok()
                .map(|st| json!({ "running": st.running, "exit_code": st.exit_code })),
            None => None,
        };
        out.push(json!({
            "id": s.id,
            "name": s.name,
            "node_id": s.node_id,
            "node_name": node_name,
            "status": status,
        }));
    }
    Json(json!(out)).into_response()
}

pub async fn api_server_status(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    require_api(app, headers, &id, "console", |app, srv, d| async move {
        match d.status(&srv.id).await {
            Ok(st) => Json(json!(st)).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_server_stats(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    require_api(app, headers, &id, "console", |app, srv, d| async move {
        match d.stats(&srv.id).await {
            Ok(v) => Json(json!(v)).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_server_logs(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    require_api(app, headers, &id, "console", |app, srv, d| async move {
        let tail: usize = q.get("tail").and_then(|t| t.parse().ok()).unwrap_or(200);
        match d.logs(&srv.id, tail).await {
            Ok(t) => Json(json!({ "lines": t.lines().collect::<Vec<_>>() })).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

#[derive(serde::Deserialize)]
struct PowerBody {
    action: String,
}

pub async fn api_server_power(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::Json(body): axum::Json<PowerBody>,
) -> Response {
    require_api(app, headers, &id, "power", |app, srv, d| async move {
        use nucleus_core::PowerAction;
        let action = match body.action.as_str() {
            "start" => PowerAction::Start,
            "stop" => PowerAction::Stop,
            "restart" => PowerAction::Restart,
            "kill" => PowerAction::Kill,
            _ => return json_err(StatusCode::BAD_REQUEST, "bad action (start|stop|restart|kill)"),
        };
        match d.power(&srv.id, action).await {
            Ok(()) => {
                perms::record(&app.db, "api", "server.power", &srv.id, &body.action);
                Json(json!({ "ok": true })).into_response()
            }
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

/// Run `f` only if the API caller is permitted on `id` for `flag`.
async fn require_api<F, Fut>(
    app: SharedApp,
    headers: HeaderMap,
    id: &str,
    flag: &str,
    f: F,
) -> Response
where
    F: FnOnce(SharedApp, ServerRow, crate::daemon::DaemonClient) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let Some(u) = api_user(&app, &headers) else {
        return json_err(StatusCode::UNAUTHORIZED, "invalid api key");
    };
    let Some(srv) = get_server(&app, id) else {
        return json_err(StatusCode::NOT_FOUND, "unknown server");
    };
    if !perms::allowed(&app.db, &u, &srv, flag) {
        return json_err(StatusCode::FORBIDDEN, "no permission on this server");
    }
    let node = match get_node(&app, &srv.node_id) {
        Some(n) => n,
        None => return json_err(StatusCode::BAD_GATEWAY, "node missing"),
    };
    let d = crate::daemon::DaemonClient::new(&app, &node);
    f(app, srv, d).await
}

pub fn json_err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Router mounted at `/api/v1`.
pub fn router(app: SharedApp) -> axum::Router {
    axum::Router::new()
        .route("/servers", get(api_list_servers))
        .route("/servers/{id}", get(api_server_status))
        .route("/servers/{id}/stats", get(api_server_stats))
        .route("/servers/{id}/logs", get(api_server_logs))
        .route("/servers/{id}/power", axum::routing::post(api_server_power))
        .with_state(app.clone())
}
