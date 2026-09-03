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

/// Run `f` only if the API caller is an admin (global operations).
async fn require_admin_api<F, Fut>(app: SharedApp, headers: HeaderMap, f: F) -> Response
where
    F: FnOnce(SharedApp, User) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let Some(u) = api_user(&app, &headers) else {
        return json_err(StatusCode::UNAUTHORIZED, "invalid api key");
    };
    if u.role != "admin" {
        return json_err(StatusCode::FORBIDDEN, "admin only");
    }
    f(app, u).await
}

pub fn json_err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

// ---------- files ----------

pub async fn api_files_list(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    require_api(app, headers, &id, "files", |app, srv, d| async move {
        match d.list_files(&srv.id, q.get("path").map(String::as_str)).await {
            Ok(v) => Json(json!(v)).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

/// Raw file contents (application/octet-stream). Write with PUT + raw body.
pub async fn api_files_content(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    method: axum::http::Method,
    body: axum::body::Bytes,
) -> Response {
    let Some(path) = q.get("path").cloned() else {
        return json_err(StatusCode::BAD_REQUEST, "missing ?path=");
    };
    if path.contains("..") {
        return json_err(StatusCode::BAD_REQUEST, "bad path");
    }
    match method {
        axum::http::Method::GET => {
            require_api(app, headers, &id, "files", |app, srv, d| async move {
                match d.read_file(&srv.id, &path).await {
                    Ok(bytes) => (
                        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                        bytes,
                    )
                        .into_response(),
                    Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
                }
            })
            .await
        }
        axum::http::Method::PUT => {
            require_api(app, headers, &id, "files", |app, srv, d| async move {
                match d.write_file(&srv.id, &path, body.to_vec()).await {
                    Ok(()) => {
                        perms::record(&app.db, "api", "file.write", &srv.id, &path);
                        Json(json!({ "ok": true })).into_response()
                    }
                    Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
                }
            })
            .await
        }
        _ => json_err(StatusCode::METHOD_NOT_ALLOWED, "use GET or PUT"),
    }
}

#[derive(serde::Deserialize)]
struct PathBody {
    path: String,
}

async fn files_mutation(
    app: SharedApp,
    headers: HeaderMap,
    id: &str,
    action: &str,
    body: axum::Json<PathBody>,
) -> Response {
    require_api(app, headers, id, "files", |app, srv, d| async move {
        let res = match action {
            "mkdir" => d.mkdir(&srv.id, &body.path).await,
            "delete" => d.delete_path(&srv.id, &body.path).await,
            _ => unreachable!(),
        };
        match res {
            Ok(()) => {
                perms::record(&app.db, "api", &format!("file.{action}"), &srv.id, &body.path);
                Json(json!({ "ok": true })).into_response()
            }
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_files_mkdir(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: axum::Json<PathBody>,
) -> Response {
    files_mutation(app, headers, &id, "mkdir", body).await
}

pub async fn api_files_delete(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: axum::Json<PathBody>,
) -> Response {
    files_mutation(app, headers, &id, "delete", body).await
}

#[derive(serde::Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

pub async fn api_files_rename(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::Json(body): axum::Json<RenameBody>,
) -> Response {
    require_api(app, headers, &id, "files", |app, srv, d| async move {
        let payload = json!({ "from": body.from, "to": body.to });
        match d.rename_path(&srv.id, &payload).await {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

// ---------- backups ----------

pub async fn api_backups_list(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    require_api(app, headers, &id, "backups", |app, srv, d| async move {
        match d.list_backups(&srv.id).await {
            Ok(v) => Json(json!(v)).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_backups_create(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    require_api(app, headers, &id, "backups", |app, srv, d| async move {
        match d.create_backup(&srv.id).await {
            Ok(v) => {
                perms::record(&app.db, "api", "backup.create", &srv.id, "");
                Json(v).into_response()
            }
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

/// Streams the archive through the panel (application/gzip).
pub async fn api_backup_download(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath((id, bid)): AxumPath<(String, String)>,
) -> Response {
    require_api(app, headers, &id, "backups", |app, srv, d| async move {
        if !bid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return json_err(StatusCode::BAD_REQUEST, "invalid backup id");
        }
        let url = d.backup_download_url(&srv.id, &bid);
        match d.get_stream(&url).await {
            Ok(r) if r.status().is_success() => {
                use axum::response::IntoResponse as _;
                let stream = r.bytes_stream();
                (
                    [(axum::http::header::CONTENT_TYPE, "application/gzip")],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            Ok(r) => json_err(
                StatusCode::BAD_GATEWAY,
                &format!("node returned {}", r.status()),
            ),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_backup_delete(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath((id, bid)): AxumPath<(String, String)>,
) -> Response {
    require_api(app, headers, &id, "backups", |app, srv, d| async move {
        match d.delete_backup(&srv.id, &bid).await {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_backup_restore(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath((id, bid)): AxumPath<(String, String)>,
) -> Response {
    require_api(app, headers, &id, "backups", |app, srv, d| async move {
        match d.restore_backup(&srv.id, &bid).await {
            Ok(()) => {
                perms::record(&app.db, "api", "backup.restore", &srv.id, &bid);
                Json(json!({ "ok": true, "note": "start the server to apply" })).into_response()
            }
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

// ---------- schedules ----------

#[derive(serde::Deserialize)]
struct ScheduleBody {
    name: String,
    cron: String,
    action: String,
    #[serde(default)]
    payload: Option<String>,
}

pub async fn api_schedules_list(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    require_api(app, headers, &id, "schedules", |app, srv, d| async move {
        match d.schedules(&srv.id).await {
            Ok(v) => Json(json!(v)).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_schedules_add(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::Json(body): axum::Json<ScheduleBody>,
) -> Response {
    require_api(app, headers, &id, "schedules", |app, srv, d| async move {
        match d
            .schedule_add(
                &srv.id,
                &body.name,
                &body.cron,
                &body.action,
                body.payload.as_deref(),
            )
            .await
        {
            Ok(v) => Json(v).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

#[derive(serde::Deserialize)]
struct ScheduleToggleBody {
    enabled: bool,
}

pub async fn api_schedules_toggle(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath((id, tid)): AxumPath<(String, String)>,
    axum::Json(body): axum::Json<ScheduleToggleBody>,
) -> Response {
    require_api(app, headers, &id, "schedules", |app, srv, d| async move {
        match d.schedule_toggle(&srv.id, &tid, body.enabled).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_schedules_delete(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath((id, tid)): AxumPath<(String, String)>,
) -> Response {
    require_api(app, headers, &id, "schedules", |app, srv, d| async move {
        match d.schedule_delete(&srv.id, &tid).await {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_schedules_run(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath((id, tid)): AxumPath<(String, String)>,
) -> Response {
    require_api(app, headers, &id, "schedules", |app, srv, d| async move {
        match d.schedule_run(&srv.id, &tid).await {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

// ---------- admin: server lifecycle ----------

#[derive(serde::Deserialize)]
struct ApiCreateBody {
    node_id: String,
    #[serde(default)]
    tags: String,
    #[serde(flatten)]
    spec: nucleus_core::CreateServerRequest,
}

pub async fn api_server_create(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ApiCreateBody>,
) -> Response {
    require_admin_api(app, headers, |app, user| async move {
        let Some(node) = get_node(&app, &body.node_id) else {
            return json_err(StatusCode::BAD_REQUEST, "unknown node_id");
        };
        let d = crate::daemon::DaemonClient::new(&app, &node);
        let spec = body.spec;
        match d.create_server(&spec).await {
            Ok(st) => {
                let _ = app.db.with(|c| {
                    c.execute(
                        r#"INSERT INTO servers (id, name, node_id, egg_slug, image, startup, env_json,
                               ports_json, mem_mb, cpu, disk_mb, pids_limit, tags,
                               stop_command, accept_eula, owner_id, created_at)
                           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"#,
                        rusqlite::params![
                            spec.id,
                            spec.name,
                            node.id,
                            Option::<String>::None,
                            spec.image,
                            spec.startup,
                            serde_json::to_string(&spec.env).unwrap_or_default(),
                            serde_json::to_string(&spec.ports).unwrap_or_default(),
                            spec.limits.mem_mb as i64,
                            spec.limits.cpu_cores,
                            spec.limits.disk_mb as i64,
                            spec.limits.pids_limit,
                            body.tags.trim(),
                            spec.stop_command,
                            spec.accept_eula as i64,
                            user.id,
                            chrono::Utc::now().timestamp()
                        ],
                    )?;
                    Ok(())
                });
                perms::record(&app.db, "api", "server.create", &spec.id, &node.id);
                Json(json!(st)).into_response()
            }
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

pub async fn api_server_delete(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    require_admin_api(app, headers, |app, _user| async move {
        let Some(srv) = get_server(&app, &id) else {
            return json_err(StatusCode::NOT_FOUND, "unknown server");
        };
        let node = match get_node(&app, &srv.node_id) {
            Some(n) => n,
            None => return json_err(StatusCode::BAD_GATEWAY, "node missing"),
        };
        let d = crate::daemon::DaemonClient::new(&app, &node);
        let purge = q.get("purge_data").map(|v| v == "true").unwrap_or(false);
        if let Err(e) = d.remove_server(&srv.id, purge).await {
            // Keep the row on node failure so the server stays manageable.
            return json_err(
                StatusCode::BAD_GATEWAY,
                &format!("{e:#} — server was NOT deleted; check the node and retry"),
            );
        }
        let _ = app.db.with(|c| {
            c.execute(
                "DELETE FROM user_servers WHERE server_id = ?1",
                rusqlite::params![srv.id],
            )?;
            c.execute(
                "DELETE FROM servers WHERE id = ?1",
                rusqlite::params![srv.id],
            )?;
            Ok(())
        });
        perms::record(&app.db, "api", "server.delete", &srv.id, &format!("purge={purge}"));
        axum::http::StatusCode::NO_CONTENT.into_response()
    })
    .await
}

pub async fn api_server_config(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    require_api(app, headers, &id, "settings", |app, srv, d| async move {
        match d.update_config(&srv.id, &body).await {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => json_err(StatusCode::BAD_GATEWAY, &e.to_string()),
        }
    })
    .await
}

/// Router mounted at `/api/v1`.
pub fn router(app: SharedApp) -> axum::Router {
    axum::Router::new()
        .route("/servers", get(api_list_servers).post(api_server_create))
        .route("/servers/{id}", get(api_server_status).delete(api_server_delete))
        .route("/servers/{id}/config", axum::routing::post(api_server_config))
        .route("/servers/{id}/stats", get(api_server_stats))
        .route("/servers/{id}/logs", get(api_server_logs))
        .route("/servers/{id}/power", axum::routing::post(api_server_power))
        .route("/servers/{id}/files", get(api_files_list))
        .route(
            "/servers/{id}/files/content",
            get(api_files_content).put(api_files_content),
        )
        .route("/servers/{id}/files/mkdir", axum::routing::post(api_files_mkdir))
        .route("/servers/{id}/files/delete", axum::routing::post(api_files_delete))
        .route("/servers/{id}/files/rename", axum::routing::post(api_files_rename))
        .route(
            "/servers/{id}/backups",
            get(api_backups_list).post(api_backups_create),
        )
        .route(
            "/servers/{id}/backups/{bid}",
            axum::routing::delete(api_backup_delete),
        )
        .route(
            "/servers/{id}/backups/{bid}/download",
            get(api_backup_download),
        )
        .route(
            "/servers/{id}/backups/{bid}/restore",
            axum::routing::post(api_backup_restore),
        )
        .route(
            "/servers/{id}/schedules",
            get(api_schedules_list).post(api_schedules_add),
        )
        .route(
            "/servers/{id}/schedules/{tid}",
            axum::routing::put(api_schedules_toggle).delete(api_schedules_delete),
        )
        .route(
            "/servers/{id}/schedules/{tid}/run",
            axum::routing::post(api_schedules_run),
        )
        .with_state(app.clone())
}
