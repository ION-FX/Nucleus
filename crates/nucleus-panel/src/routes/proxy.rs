use super::pages::nav_ctx;
use super::*;
use crate::daemon::DaemonClient;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Form, Multipart, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use futures_util::{SinkExt, StreamExt};

fn require_login(app: &App, headers: &HeaderMap) -> Option<()> {
    let (email, _) = nav_ctx(app, headers);
    if email.is_empty() {
        None
    } else {
        Some(())
    }
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
    if require_login(&app, &headers).is_none() {
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
        Ok(()) => Redirect::to(&format!("/servers/{id}")).into_response(),
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let purge = form.purge_data.as_deref() == Some("1");
    let res = d.remove_server(&srv.id, purge).await;
    let _ = app.db.with(|c| {
        c.execute(
            "DELETE FROM servers WHERE id = ?1",
            rusqlite::params![srv.id],
        )?;
        Ok(())
    });
    match res {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("delete failed: {e:#} — <a href='/'>dashboard</a>"),
        )
            .into_response(),
    }
}

// ---------- console websocket relay ----------

pub async fn ws_relay(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if require_login(&app, &headers).is_none() {
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let path = join_path(&form.cwd, &form.dir);
    if let Err(e) = d.mkdir(&srv.id, &path).await {
        return (StatusCode::BAD_GATEWAY, format!("mkdir failed: {e:#}")).into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.delete_path(&srv.id, &form.path).await {
        return (StatusCode::BAD_GATEWAY, format!("delete failed: {e:#}")).into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
            _ => continue,
        }
    }

    let Some((fname, data)) = upload else {
        return (StatusCode::BAD_REQUEST, "no file provided").into_response();
    };
    let dest = join_path(&cwd, &fname);
    if let Err(e) = d.write_file(&srv.id, &dest, data).await {
        return (StatusCode::BAD_GATEWAY, format!("upload failed: {e:#}")).into_response();
    }
    Redirect::to(&format!(
        "/servers/{id}/files?path={}",
        urlencoding::encode(&cwd)
    ))
    .into_response()
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
        Ok(()) => Redirect::to(&format!(
            "/servers/{id}/files?path={}",
            urlencoding::encode(&parent_dir(&dest))
        ))
        .into_response(),
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.sftp_reset(&srv.id).await {
        return (StatusCode::BAD_GATEWAY, format!("reset failed: {e:#}")).into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
        Ok(()) => Redirect::to(&format!("/servers/{id}")).into_response(),
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.create_backup(&srv.id).await {
        Ok(_) => Redirect::to(&format!("/servers/{id}")).into_response(),
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.delete_backup(&srv.id, &form.bid).await {
        return (StatusCode::BAD_GATEWAY, format!("delete failed: {e:#}")).into_response();
    }
    Redirect::to(&format!("/servers/{id}")).into_response()
}

#[derive(serde::Deserialize)]
pub struct BackupDownloadQuery {
    pub bid: String,
}

pub async fn backup_download(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<BackupDownloadQuery>,
    headers: HeaderMap,
) -> Response {
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d.rerun_install_script(&srv.id).await {
        Ok(()) => Redirect::to(&format!("/servers/{id}/modpacks")).into_response(),
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    match d
        .schedule_add(&srv.id, &form.name, &form.cron, &form.action, form.payload.as_deref())
        .await
    {
        Ok(_) => Redirect::to(&format!("/servers/{id}/schedules")).into_response(),
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    let enabled = form.enabled == "1" || form.enabled == "true";
    if let Err(e) = d.schedule_toggle(&srv.id, &form.tid, enabled).await {
        return (StatusCode::BAD_GATEWAY, format!("toggle failed: {e:#}")).into_response();
    }
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
    if require_login(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let Ok((srv, d)) = daemon_for_server(&app, &id).await else {
        return (StatusCode::NOT_FOUND, "no such server").into_response();
    };
    if let Err(e) = d.schedule_delete(&srv.id, &form.tid).await {
        return (StatusCode::BAD_GATEWAY, format!("delete failed: {e:#}")).into_response();
    }
    Redirect::to(&format!("/servers/{id}/schedules")).into_response()
}
