pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let v = v.strip_prefix("Bearer ")?;
    Some(v.to_string())
}

use super::pages::nav_ctx;
use super::*;
use crate::daemon::DaemonClient;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Form, Multipart, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use futures_util::{SinkExt, StreamExt};

/// Authenticated user with at least `flag` permission on server `id`.
fn require_perm(
    app: &App,
    headers: &HeaderMap,
    id: &str,
    flag: &str,
) -> Option<crate::models::User> {
    let user = crate::auth::Sessions::user_for(&app.db, headers)?;
    let srv = get_server(app, id)?;
    crate::perms::allowed(&app.db, &user, &srv, flag).then_some(user)
}

async fn daemon_for_server(
    app: &App,
    id: &str,
) -> Result<(crate::models::ServerRow, DaemonClient), Response> {
    let Some(srv) = get_server(app, id) else {
        return Err((StatusCode::NOT_FOUND, "no such server").into_response());
    };
    let Some(node) = get_node(app, &srv.node_id) else {
        return Err((StatusCode::BAD_GATEWAY, "node missing").into_response());
    };
    Ok((srv, DaemonClient::new(app.http.clone(), &node)))
}

pub async fn server_stats(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if require_perm(&app, &headers, &id, "console").is_none() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.stats(&srv.id).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => axum::Json(serde_json::json!({
            "running": false, "cpu_percent": 0.0,
            "mem_used_mb": 0.0, "mem_limit_mb": 0.0, "mem_percent": 0.0,
            "net_rx_kbps": 0.0, "net_tx_kbps": 0.0,
            "error": e.to_string(),
        }))
        .into_response(),
    }
}

// ---------- power / delete ----------

pub async fn power(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "power") else {
        return Redirect::to("/login").into_response();
    };
    let action = form
        .iter()
        .find(|(k, _)| k == "action")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let action = match action {
        "start" => nucleus_core::PowerAction::Start,
        "stop" => nucleus_core::PowerAction::Stop,
        "kill" => nucleus_core::PowerAction::Kill,
        "restart" => nucleus_core::PowerAction::Restart,
        _ => return (StatusCode::BAD_REQUEST, "bad action").into_response(),
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.power(&srv.id, action).await {
        Ok(()) => {
            crate::perms::record(
                &app.db,
                &user.email,
                "server.power",
                &id,
                &format!("{action:?}"),
            );
            Redirect::to(&format!("/servers/{id}")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("power failed: {e:#} — <a href='/servers/{id}'>back</a>"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    pub purge_data: Option<String>,
}

pub async fn delete_server(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Response {
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if !crate::perms::is_owner_or_admin(&app.db, &user, &srv) {
        return (StatusCode::FORBIDDEN, "only the owner or an admin can delete this server")
            .into_response();
    }
    let purge = form.purge_data.as_deref() == Some("1");
    let res = d.remove_server(&srv.id, purge).await;
    crate::perms::record(
        &app.db,
        &user.email,
        "server.delete",
        &srv.name,
        &match &res {
            Ok(()) => format!("{} (purge={})", srv.id, purge),
            Err(e) => format!("{} FAILED (purge={}): {e:#}", srv.id, purge),
        },
    );
    if let Err(e) = res {
        // The node still holds the container/data — dropping the row now
        // would orphan it (invisible in the panel but alive on the node).
        return (
            StatusCode::BAD_GATEWAY,
            format!(
                "delete failed: {e:#} — the server was NOT deleted; check the node and retry — <a href='/servers/{id}/settings'>back</a>"
            ),
        )
            .into_response();
    }
    // clean up memberships
    let _ = app.db.with(|c| {
        c.execute("DELETE FROM user_servers WHERE server_id = ?1", rusqlite::params![srv.id])?;
        Ok(())
    });
    let _ = app.db.with(|c| {
        c.execute(
            "DELETE FROM servers WHERE id = ?1",
            rusqlite::params![srv.id],
        )?;
        Ok(())
    });
    Redirect::to("/").into_response()
}

// ---------- server transfer between nodes ----------

#[derive(serde::Deserialize)]
pub struct TransferForm {
    pub target_node: String,
}

pub async fn transfer_server(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<TransferForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "settings") else {
        return Redirect::to("/login").into_response();
    };
    let Some(srv) = get_server(&app, &id) else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if form.target_node == srv.node_id {
        return Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode("Already on that node."))).into_response();
    }
    let Some(target) = get_node(&app, &form.target_node) else {
        return Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode("Unknown target node."))).into_response();
    };
    let src = DaemonClient::new(app.http.clone(), &get_node(&app, &srv.node_id).unwrap());
    let dst = DaemonClient::new(app.http.clone(), &target);

    // 1) stop the source container so we get a consistent snapshot.
    let _ = src.power(&srv.id, nucleus_core::PowerAction::Stop).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // 2) make a backup on the source and pull the archive here.
    let bid = match src.create_backup(&srv.id).await {
        Ok(v) => v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        Err(e) => return Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode(&format!("Snapshot failed: {e}")))).into_response(),
    };
    let bytes = match src.download_backup_bytes(&srv.id, &bid).await {
        Ok(b) => b,
        Err(e) => return Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode(&format!("Transfer failed (download): {e}")))).into_response(),
    };

    // 3) recreate the server definition on the destination with the same id.
    let env: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&srv.env_json).unwrap_or_default();
    let ports: Vec<nucleus_core::PortMapping> =
        serde_json::from_str(&srv.ports_json).unwrap_or_default();
    let spec = nucleus_core::CreateServerRequest {
        id: srv.id.clone(),
        name: srv.name.clone(),
        image: srv.image.clone(),
        startup: srv.startup.clone(),
        env,
        ports,
        limits: nucleus_core::Limits { mem_mb: srv.mem_mb, cpu_cores: srv.cpu, disk_mb: srv.disk_mb, pids_limit: srv.pids_limit },
        stop_command: srv.stop_command.clone(),
        accept_eula: srv.accept_eula,
        install_script: None,
        installer_image: None,
    };
    if let Err(e) = dst.create_server(&spec).await {
        // try to roll the source back up
        let _ = src.power(&srv.id, nucleus_core::PowerAction::Start).await;
        return Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode(&format!("Dest create failed: {e}")))).into_response();
    }

    // 4) push the archive into the destination and extract it.
    if let Err(e) = dst.upload_transfer(&srv.id, bytes.to_vec()).await {
        let _ = src.power(&srv.id, nucleus_core::PowerAction::Start).await;
        let _ = dst.remove_server(&srv.id, true).await;
        return Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode(&format!("Transfer failed (upload): {e}")))).into_response();
    }

    // 5) repoint the DB row and tear down the source.
    let _ = app.db.with(|c| {
        c.execute("UPDATE servers SET node_id=?1 WHERE id=?2", rusqlite::params![target.id, srv.id])?;
        Ok(())
    });
    let _ = src.remove_server(&srv.id, true).await;

    crate::perms::record(&app.db, &user.email, "server.transfer", &srv.id, &format!("to {}", target.name));
    Redirect::to(&format!("/servers/{id}/settings?msg={}", urlencoding::encode("Server transferred."))).into_response()
}

// ---------- console websocket relay ----------

pub async fn ws_relay(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if require_perm(&app, &headers, &id, "console").is_none() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let (srv, d) = match daemon_for_server(&app, &id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    ws.on_upgrade(move |socket| async move {
        relay(socket, d, srv.id).await;
    })
}

async fn relay(socket: WebSocket, d: DaemonClient, server_id: String) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let url = d.ws_console_url(&server_id);
    let mut req = match url.clone().into_client_request() {
        Ok(r) => r,
        Err(_) => return,
    };
    let auth =
        match tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&d.auth_header_value()) {
            Ok(v) => v,
            Err(_) => return,
        };
    req.headers_mut().insert("authorization", auth);

    let (upstream, _resp) = match tokio_tungstenite::connect_async(req).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, %url, "console upstream connect failed");
            return;
        }
    };

    let (mut client_tx, mut client_rx) = socket.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    let to_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = up_rx.next().await {
            let out = match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => WsMessage::Text(t.into()),
                tokio_tungstenite::tungstenite::Message::Binary(b) => WsMessage::Binary(b.into()),
                tokio_tungstenite::tungstenite::Message::Ping(p) => WsMessage::Ping(p.into()),
                tokio_tungstenite::tungstenite::Message::Pong(p) => WsMessage::Pong(p.into()),
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => continue,
            };
            if client_tx.send(out).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    });

    while let Some(Ok(msg)) = client_rx.next().await {
        let out = match msg {
            WsMessage::Text(t) => tokio_tungstenite::tungstenite::Message::Text(t.to_string()),
            WsMessage::Binary(b) => tokio_tungstenite::tungstenite::Message::Binary(b.to_vec()),
            WsMessage::Close(_) | WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
        };
        if up_tx.send(out).await.is_err() {
            break;
        }
    }
    to_client.abort();
}

// ---------- files ops ----------

#[derive(serde::Deserialize)]
pub struct PathOnlyQuery {
    #[serde(default)]
    pub path: Option<String>,
}

pub async fn file_download(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PathOnlyQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return Redirect::to("/login").into_response();
    };
    let Some(path) = q.path else {
        return (StatusCode::BAD_REQUEST, "missing ?path").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.read_file(&srv.id, &path).await {
        Ok(bytes) => {
            let fname = path.rsplit('/').next().unwrap_or("file");
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{}\"", fname.replace('"', "")),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("download failed: {e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct MkdirForm {
    pub dir: String,
    pub cwd: String,
}

pub async fn files_mkdir(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<MkdirForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let path = join_path(&form.cwd, &form.dir);
    if let Err(e) = d.mkdir(&srv.id, &path).await {
        return (StatusCode::BAD_GATEWAY, format!("mkdir failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "file.mkdir", &id, "");
    Redirect::to(&format!(
        "/servers/{id}/files?path={}",
        urlencoding::encode(&path)
    ))
    .into_response()
}

#[derive(serde::Deserialize)]
pub struct DeleteFileForm {
    pub path: String,
}

pub async fn files_delete(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<DeleteFileForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.delete_path(&srv.id, &form.path).await {
        return (StatusCode::BAD_GATEWAY, format!("delete failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "file.delete", &id, "");
    Redirect::to(&format!(
        "/servers/{id}/files?path={}",
        urlencoding::encode(&parent_dir(&form.path))
    ))
    .into_response()
}

pub async fn files_upload(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };

    let mut cwd = "/".to_string();
    let mut upload: Option<(String, Vec<u8>)> = None;
    while let Some(field) = mp.next_field().await.ok().flatten() {
        match field.name().unwrap_or("") {
            "cwd" => {
                if let Ok(v) = field.text().await {
                    cwd = v;
                }
            }
            "file" => {
                let fname = field
                    .file_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "upload.bin".into());
                match field.bytes().await {
                    Ok(b) => upload = Some((fname, b.to_vec())),
                    Err(e) => {
                        return (StatusCode::BAD_REQUEST, format!("read upload: {e}"))
                            .into_response()
                    }
                }
            }
            _ => {}
        }
    }

    let Some((fname, data)) = upload else {
        return (StatusCode::BAD_REQUEST, "no file provided").into_response();
    };
    let dest = join_path(&cwd, &fname);
    if let Err(e) = d.write_file(&srv.id, &dest, data).await {
        return (StatusCode::BAD_GATEWAY, format!("upload failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "file.upload", &id, "");
    Redirect::to(&format!(
        "/servers/{id}/files?path={}",
        urlencoding::encode(&cwd)
    ))
    .into_response()
}

// ---------- mod browser (Modrinth) ----------

pub async fn mods_search(
    State(app): State<SharedApp>,
    AxumPath(_id): AxumPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if crate::auth::Sessions::user_for(&app.db, &headers).is_none() {
        return (StatusCode::FORBIDDEN, "not logged in").into_response();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    let loader = q.get("loader").cloned().unwrap_or_else(|| "fabric".into());
    let gv = q.get("game_version").cloned().unwrap_or_default();

    let mut facets = vec![
        "\"project_type:mod\"".to_string(),
        format!("\"categories:{}\"", loader),
    ];
    if !gv.is_empty() && gv != "*" {
        facets.push(format!("\"versions:{}\"", gv));
    }
    let facets_json = format!("[{}]", facets
        .iter()
        .map(|f| format!("[{f}]"))
        .collect::<Vec<_>>()
        .join(","));

    match app
        .http
        .get("https://api.modrinth.com/v2/search")
        .header("User-Agent", "Nucleus/1.0")
        .query(&[
            ("query", query.as_str()),
            ("facets", facets_json.as_str()),
            ("limit", "20"),
        ])
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_else(|_| "{}".into());
            if status.is_success() {
                (
                    [(header::CONTENT_TYPE, "application/json".to_string())],
                    body,
                )
                    .into_response()
            } else {
                (StatusCode::BAD_GATEWAY, format!("Modrinth API error: {status}")).into_response()
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("Modrinth request failed: {e}")).into_response(),
    }
}

pub async fn mods_versions(
    State(app): State<SharedApp>,
    AxumPath(_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if crate::auth::Sessions::user_for(&app.db, &headers).is_none() {
        return (StatusCode::FORBIDDEN, "not logged in").into_response();
    }
    match app
        .http
        .get("https://api.modrinth.com/v2/tag/game_version")
        .header("User-Agent", "Nucleus/1.0")
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return (StatusCode::BAD_GATEWAY, format!("Modrinth API error: {}", resp.status())).into_response();
            }
            #[derive(serde::Deserialize)]
            struct GhVersion {
                version: String,
                version_type: String,
            }
            let all: Vec<GhVersion> = match resp.json().await {
                Ok(v) => v,
                Err(e) => return (StatusCode::BAD_GATEWAY, format!("parse failed: {e}")).into_response(),
            };
            // release versions only, newest first (API already returns them sorted)
            let releases: Vec<serde_json::Value> = all
                .into_iter()
                .filter(|v| v.version_type == "release")
                .map(|v| serde_json::json!({"version": v.version}))
                .collect();
            (
                [(header::CONTENT_TYPE, "application/json".to_string())],
                serde_json::to_string(&releases).unwrap_or_else(|_| "[]".into()),
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("Modrinth request failed: {e}")).into_response(),
    }
}

pub async fn mods_install(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<ModInstallReq>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return (StatusCode::FORBIDDEN, "no file permission").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };

    // Fetch the project versions to find the latest file URL.
    // Omit filters the user didn't pick so we always get a result when possible.
    let mut params: Vec<String> = Vec::new();
    if !req.game_version.is_empty() && req.game_version != "*" {
        params.push(format!("game_versions=[\"{}\"]", urlencoding::encode(&req.game_version)));
    }
    if !req.loader.is_empty() && req.loader != "*" {
        params.push(format!("loaders=[\"{}\"]", urlencoding::encode(&req.loader)));
    }
    let url = format!(
        "https://api.modrinth.com/v2/project/{}/version{}",
        urlencoding::encode(&req.project_id),
        if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) }
    );
    let resp = match app.http.get(&url).header("User-Agent", "Nucleus/1.0").send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("Modrinth request failed: {e}")).into_response(),
    };
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("read failed: {e}")).into_response(),
    };
    let versions: Vec<serde_json::Value> = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("parse failed: {e}")).into_response(),
    };
    let Some(ver) = versions.first() else {
        return (StatusCode::NOT_FOUND, "no compatible version found").into_response();
    };
    let files = ver.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    let Some(file) = files.first() else {
        return (StatusCode::NOT_FOUND, "no file in version").into_response();
    };
    let dl_url = file.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let filename = file.get("filename").and_then(|f| f.as_str()).unwrap_or("mod.jar");
    if dl_url.is_empty() {
        return (StatusCode::NOT_FOUND, "no download URL").into_response();
    }

    let target_path = format!("/mods/{}", filename);
    match d.fetch_file(&srv.id, dl_url, &target_path).await {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "mod.install", &id, &filename);
            axum::Json(serde_json::json!({"ok": true, "file": filename, "path": target_path})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("install failed: {e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct ModInstallReq {
    pub project_id: String,
    pub game_version: String,
    pub loader: String,
}

// ---------- file manager extras ----------

pub async fn files_rename(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<RenameReq>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return (StatusCode::FORBIDDEN, "no file permission").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let body = serde_json::json!({"from": req.from, "to": req.to});
    match d.rename_path(&srv.id, &body).await {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "file.rename", &id, &req.from);
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("rename failed: {e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct RenameReq {
    pub from: String,
    pub to: String,
}

pub async fn files_move(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<MoveReq>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return (StatusCode::FORBIDDEN, "no file permission").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let body = serde_json::json!({"from": req.from, "to": req.to});
    match d.rename_path(&srv.id, &body).await {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "file.move", &id, &req.from);
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("move failed: {e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct MoveReq {
    pub from: String,
    pub to: String,
}

pub async fn files_archive(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<ArchiveReq>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return (StatusCode::FORBIDDEN, "no file permission").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.archive(&srv.id, &req).await {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "file.archive", &id, &req.path);
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("archive failed: {e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct ArchiveReq {
    pub path: String,
    pub action: String,
}

pub async fn files_extract(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<ArchiveReq>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return (StatusCode::FORBIDDEN, "no file permission").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.extract(&srv.id, &req).await {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "file.extract", &id, &req.path);
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("extract failed: {e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct FetchFileForm {
    pub url: String,
    pub path: String,
}

pub async fn files_fetch(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<FetchFileForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let dir = form.path.trim_end_matches('/');
    let dest = match dir.rsplit('/').next() {
        // Path already names a file (e.g. "/server.jar").
        Some(last) if last.contains('.') && !last.is_empty() => dir.to_string(),
        _ => {
            let fname = form
                .url
                .split('?')
                .next()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("download.bin");
            format!("{}/{}", if dir.is_empty() { "" } else { dir }, fname)
        }
    };
    match d.fetch_file(&srv.id, &form.url, &dest).await {
        Ok(()) => {
            crate::perms::record(&app.db, &user.email, "file.fetch", &id, "");
            Redirect::to(&format!(
                "/servers/{id}/files?path={}",
                urlencoding::encode(&parent_dir(&dest))
            ))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("download failed: {e:#} — <a href='/servers/{id}/files'>back</a>"),
        )
            .into_response(),
    }
}

pub async fn sftp_reset(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "files") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.sftp_reset(&srv.id).await {
        return (StatusCode::BAD_GATEWAY, format!("reset failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "sftp.reset", &id, "");
    Redirect::to(&format!("/servers/{id}/files")).into_response()
}

fn join_path(dir: &str, name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

fn parent_dir(path: &str) -> String {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => t[..i].to_string(),
        None => "/".into(),
    }
}

// ---------- modpack install ----------

pub async fn install_pack(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "modpacks") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let mut pack: Option<(String, Vec<u8>)> = None;
    while let Some(field) = mp.next_field().await.ok().flatten() {
        if field.name() == Some("pack") {
            let fname = field
                .file_name()
                .map(str::to_owned)
                .unwrap_or_else(|| "pack.zip".into());
            match field.bytes().await {
                Ok(b) => pack = Some((fname, b.to_vec())),
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, format!("read pack: {e}")).into_response()
                }
            }
        }
    }
    let Some((fname, data)) = pack else {
        return (StatusCode::BAD_REQUEST, "no pack uploaded").into_response();
    };
    tracing::info!(server = %srv.id, %fname, size = data.len(), "installing pack");
    match d.upload_pack(&srv.id, &fname, data).await {
        Ok(()) => {
            crate::perms::record(&app.db, &user.email, "install.pack", &id, &fname);
            Redirect::to(&format!("/servers/{id}")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("install failed: {e:#} — <a href='/servers/{id}'>back</a>"),
        )
            .into_response(),
    }
}

// ---------- backups ----------

pub async fn backup_create(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "backups") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.create_backup(&srv.id).await {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "backup.create", &id, "");
            Redirect::to(&format!("/servers/{id}")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("backup failed: {e:#} — <a href='/servers/{id}'>back</a>"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct BidForm {
    pub bid: String,
}

pub async fn backup_delete(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<BidForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "backups") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.delete_backup(&srv.id, &form.bid).await {
        return (StatusCode::BAD_GATEWAY, format!("delete failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "backup.delete", &id, &form.bid);
    Redirect::to(&format!("/servers/{id}")).into_response()
}

#[derive(serde::Deserialize)]
pub struct BackupDownloadQuery {
    pub bid: String,
}

#[derive(serde::Deserialize)]
pub struct BackupRestoreForm {
    pub bid: String,
}

pub async fn backup_restore(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<BackupRestoreForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "backups") else {
        return Redirect::to("/login").into_response();
    };
    let bid = form.bid;
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.restore_backup(&srv.id, &bid).await {
        Ok(()) => {
            crate::perms::record(&app.db, &user.email, "backup.restore", &id, &bid);
            Redirect::to(&format!("/servers/{id}/backups")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("restore failed: {e:#} — <a href='/servers/{id}/backups'>back</a>"),
        )
            .into_response(),
    }
}

pub async fn backup_download(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<BackupDownloadQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "backups") else {
        return Redirect::to("/login").into_response();
    };
    let Some(srv) = get_server(&app, &id) else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let Some(node) = get_node(&app, &srv.node_id) else {
        return (StatusCode::BAD_GATEWAY, "node missing").into_response();
    };
    let d = DaemonClient::new(app.http.clone(), &node);
    let url = d.backup_download_url(&srv.id, &q.bid);
    let client = reqwest::Client::new();
    let resp = match client
        .get(url)
        .bearer_auth(d.auth_header_value())
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node returned {}", r.status()),
            )
                .into_response()
        }
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("fetch failed: {e}")).into_response(),
    };
    let stream = resp.bytes_stream();
    axum::body::Body::from_stream(stream).into_response()
}

// ---------- ai sysadmin trigger ----------

#[derive(serde::Deserialize)]
pub struct AiIncident {
    pub summary: String,
    pub finished_at: String,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(serde::Deserialize)]
pub struct DiagnoseForm {
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn ai_diagnose(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<DiagnoseForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "ai") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.ai_diagnose(&srv.id, form.note.as_deref()).await {
        Ok(report) => {
            let short: String = report["summary"]
                .as_str()
                .unwrap_or("done")
                .chars()
                .take(120)
                .collect();
            crate::perms::record(&app.db, &user.email, "ai.diagnose", &id, &short);
            Redirect::to(&format!(
                "/servers/{}?error={}",
                id,
                urlencoding::encode(&format!("AI diagnosis complete: {short}"))
            ))
            .into_response()
        }
        Err(e) => Redirect::to(&format!(
            "/servers/{}?error={}",
            id,
            urlencoding::encode(&format!("AI diagnosis failed: {e:#}"))
        ))
        .into_response(),
    }
}

pub async fn install_script_rerun(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "modpacks") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    // Re-run with the egg's current installer image — heals servers created
    // from stale egg imports that had no installer image stored.
    const DEFAULT_INSTALLER: &str = "ghcr.io/ptero-eggs/installers:debian";
    let image = srv.egg_slug.as_deref().and_then(|slug| {
        crate::routes::list_eggs(&app)
            .into_iter()
            .find(|e| e.slug == slug)
            .map(|e| e.egg.installer_image.clone().unwrap_or_else(|| DEFAULT_INSTALLER.to_string()))
    });
    match d.rerun_install_script(&srv.id, image).await {
        Ok(()) => {
            crate::perms::record(&app.db, &user.email, "install.script_rerun", &id, "");
            Redirect::to(&format!("/servers/{id}/modpacks")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("install failed: {e:#} — <a href='/servers/{id}/modpacks'>back</a>"),
        )
            .into_response(),
    }
}

// ---------- schedules ----------

#[derive(serde::Deserialize)]
pub struct ScheduleAddForm {
    pub name: String,
    pub cron: String,
    pub action: String,
    #[serde(default)]
    pub payload: Option<String>,
}

pub async fn schedule_add(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<ScheduleAddForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "schedules") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d
        .schedule_add(&srv.id, &form.name, &form.cron, &form.action, form.payload.as_deref())
        .await
    {
        Ok(_) => {
            crate::perms::record(&app.db, &user.email, "schedule.add", &id, &form.name);
            Redirect::to(&format!("/servers/{id}/schedules")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("could not create schedule: {e:#} — <a href='/servers/{id}/schedules'>back</a>"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct ScheduleToggleForm {
    pub tid: String,
    pub enabled: String,
}

pub async fn schedule_toggle(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<ScheduleToggleForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "schedules") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let enabled = form.enabled == "1" || form.enabled == "true";
    if let Err(e) = d.schedule_toggle(&srv.id, &form.tid, enabled).await {
        return (StatusCode::BAD_GATEWAY, format!("toggle failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "schedule.toggle", &id, &form.tid);
    Redirect::to(&format!("/servers/{id}/schedules")).into_response()
}

#[derive(serde::Deserialize)]
pub struct ScheduleDeleteForm {
    pub tid: String,
}

pub async fn schedule_delete(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<ScheduleDeleteForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "schedules") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.schedule_delete(&srv.id, &form.tid).await {
        return (StatusCode::BAD_GATEWAY, format!("delete failed: {e:#}")).into_response();
    }
    crate::perms::record(&app.db, &user.email, "schedule.delete", &id, &form.tid);
    Redirect::to(&format!("/servers/{id}/schedules")).into_response()
}

#[derive(serde::Deserialize)]
pub struct ScheduleRunForm {
    pub tid: String,
}

pub async fn schedule_run(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<ScheduleRunForm>,
) -> Response {
    let Some(user) = require_perm(&app, &headers, &id, "schedules") else {
        return Redirect::to("/login").into_response();
    };
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.schedule_run(&srv.id, &form.tid).await {
        Ok(()) => {
            crate::perms::record(&app.db, &user.email, "schedule.run", &id, &form.tid);
            Redirect::to(&format!("/servers/{id}/schedules")).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("run failed: {e:#} — <a href='/servers/{id}/schedules'>back</a>"),
        )
            .into_response(),
    }
}
