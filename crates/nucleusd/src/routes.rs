use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use nucleus_core::PowerRequest;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct LogsQuery {
    pub tail: Option<usize>,
}

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct FilenameQuery {
    pub filename: Option<String>,
}

#[derive(Deserialize)]
pub struct PurgeQuery {
    pub purge_data: Option<bool>,
}

async fn auth(State(state): State<Arc<AppState>>, req: Request<Body>, next: Next) -> Response {
    let expected = format!("Bearer {}", state.cfg.token);
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "{\"error\":\"unauthorized\"}").into_response()
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/pack/inspect", post(inspect_pack))
        .route("/servers", post(create_server).get(list_servers))
        .route("/servers/{id}", get(server_status).delete(remove_server))
        .route("/servers/{id}/config", post(update_config))
        .route("/servers/{id}/power", post(power))
        .route("/servers/{id}/console", post(console_cmd))
        .route("/servers/{id}/logs", get(logs))
        .route("/servers/{id}/ws", get(crate::console::console_ws))
        .route("/servers/{id}/files/list", get(list_files))
        .route("/servers/{id}/files/read", get(read_file))
        .route("/servers/{id}/files/write", put(write_file))
        .route("/servers/{id}/files/upload", put(upload_file))
        .route("/servers/{id}/files/mkdir", post(mkdir))
        .route("/servers/{id}/files/delete", post(delete_path))
        .route("/servers/{id}/files/rename", post(rename_path))
        .route("/servers/{id}/files/fetch", post(fetch_file))
        .route("/servers/{id}/install/pack", post(install_pack))
        .route("/servers/{id}/install/script", post(rerun_script))
        .route("/servers/{id}/install/status", get(install_status))
        .route(
            "/servers/{id}/backups",
            post(create_backup).get(list_backups),
        )
        .route(
            "/servers/{id}/backups/{bid}",
            get(download_backup).delete(delete_backup),
        )
        .route("/servers/{id}/ai/diagnose", post(ai_diagnose))
        .route("/servers/{id}/ai/incidents", get(ai_incidents))
        .route("/servers/{id}/sftp", get(sftp_info))
        .route("/servers/{id}/sftp/reset", post(sftp_reset))
        .route("/servers/{id}/stats", get(server_stats))
        .route(
            "/servers/{id}/schedules",
            get(schedules_list).post(schedules_add),
        )
        .route(
            "/servers/{id}/schedules/{tid}",
            put(schedules_update).delete(schedules_delete),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .route("/health", get(health))
        .nest("/api", api)
        .with_state(state)
}

async fn inspect_pack(State(_state): State<Arc<AppState>>, body: axum::body::Bytes) -> Response {
    if body.is_empty() {
        return err(anyhow::anyhow!("empty body"));
    }
    match crate::installer::inspect_pack(&body) {
        Ok(insight) => axum::Json(insight).into_response(),
        Err(e) => err(e),
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn create_server(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<nucleus_core::CreateServerRequest>,
) -> Response {
    match crate::docker::create_server(state, req).await {
        Ok(st) => axum::Json(st).into_response(),
        Err(e) => err(e),
    }
}

async fn list_servers(State(state): State<Arc<AppState>>) -> Response {
    axum::Json(crate::docker::list_servers(state).await).into_response()
}

async fn server_status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match crate::docker::status(state, &id).await {
        Ok(st) => axum::Json(st).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct ConfigUpdate {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    startup: Option<String>,
    #[serde(default)]
    stop_command: Option<Option<String>>,
    #[serde(default)]
    limits: Option<nucleus_core::Limits>,
    #[serde(default)]
    ports: Option<Vec<nucleus_core::PortMapping>>,
}

async fn update_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<ConfigUpdate>,
) -> Response {
    let Ok(old) = state.get(&id) else {
        return err(anyhow::anyhow!("unknown server {id}"));
    };
    if old.running.load(std::sync::atomic::Ordering::Relaxed) {
        return err(anyhow::anyhow!("stop the server before editing its config"));
    }

    let mut spec = old.spec.clone();
    if let Some(n) = req.name {
        spec.name = n;
    }
    if let Some(i) = req.image {
        spec.image = i;
    }
    if let Some(s) = req.startup {
        spec.startup = s;
    }
    if let Some(sc) = req.stop_command {
        spec.stop_command = sc;
    }
    if let Some(l) = req.limits {
        spec.limits = l;
    }
    if let Some(p) = req.ports {
        spec.ports = p;
    }

    // Rebuild the runtime entry, carrying over console history.
    let new_rt = std::sync::Arc::new(crate::state::ServerRuntime::new(spec));
    *new_rt.ring.lock().unwrap() = old.ring.lock().unwrap().clone();
    state.servers.insert(id.clone(), new_rt);
    crate::state::save_registry(&state.cfg, &state.servers);

    match state.get(&id) {
        Ok(rt) => axum::Json(rt.status()).into_response(),
        Err(e) => err(e),
    }
}

async fn remove_server(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PurgeQuery>,
) -> Response {
    match crate::docker::remove_server(state, &id, q.purge_data.unwrap_or(false)).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(e),
    }
}

async fn power(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<PowerRequest>,
) -> Response {
    match crate::docker::power(state, &id, req.action, req.command).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct ConsoleReq {
    command: String,
}

async fn console_cmd(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<ConsoleReq>,
) -> Response {
    match crate::docker::send_command(state, &id, &req.command).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => err(e),
    }
}

async fn logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> Response {
    match state.get(&id) {
        Ok(rt) => rt
            .recent_logs(q.tail.unwrap_or(500))
            .join("\n")
            .into_response(),
        Err(e) => err(e),
    }
}

async fn list_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Response {
    match crate::files::list_files(state, id, q.path).await {
        Ok(entries) => axum::Json(entries).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn read_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Response {
    let Some(path) = q.path else {
        return err(anyhow::anyhow!("missing ?path"));
    };
    match crate::files::read_file(state, id, path).await {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

async fn write_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(path) = q.path else {
        return err(anyhow::anyhow!("missing ?path"));
    };
    match crate::files::write_file(state, id, path, body).await {
        Ok(s) => s.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(path) = q.path else {
        return err(anyhow::anyhow!("missing ?path"));
    };
    match crate::files::write_file(state, id, path, body).await {
        Ok(s) => s.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn mkdir(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<crate::files::MkdirReq>,
) -> Response {
    match crate::files::mkdir(state, id, req).await {
        Ok(s) => s.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_path(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<crate::files::DeleteReq>,
) -> Response {
    match crate::files::delete(state, id, req).await {
        Ok(s) => s.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn rename_path(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<crate::files::RenameReq>,
) -> Response {
    match crate::files::rename(state, id, req).await {
        Ok(s) => s.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn fetch_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<crate::files::FetchReq>,
) -> Response {
    match crate::files::fetch_file(state, id, req).await {
        Ok(s) => s.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn install_pack(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<FilenameQuery>,
    body: axum::body::Bytes,
) -> Response {
    let filename = q.filename.unwrap_or_else(|| "pack.zip".into());
    if body.is_empty() {
        return err(anyhow::anyhow!(
            "empty body; send the pack zip as raw bytes"
        ));
    }
    // Basic sanity: zip magic number.
    if body.len() < 4 || &body[..2] != b"PK" {
        return err(anyhow::anyhow!("file is not a zip archive"));
    }
    match crate::installer::start_pack_install(state, &id, filename, body) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => err(e),
    }
}

async fn install_status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    axum::Json(crate::installer::install_status(&state, &id)).into_response()
}

async fn create_backup(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match crate::backups::create_backup(state, id).await {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => err(e),
    }
}

async fn list_backups(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match crate::backups::list_backups(state, id).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => err(e),
    }
}

async fn download_backup(
    State(state): State<Arc<AppState>>,
    Path((id, bid)): Path<(String, String)>,
) -> Response {
    match crate::backups::download_backup(state, id, bid).await {
        Ok(r) => r,
        Err(e) => err(e),
    }
}

async fn delete_backup(
    State(state): State<Arc<AppState>>,
    Path((id, bid)): Path<(String, String)>,
) -> Response {
    match crate::backups::delete_backup(state, id, bid).await {
        Ok(s) => s.into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct DiagnoseReq {
    #[serde(default)]
    note: Option<String>,
}

async fn ai_diagnose(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let note = serde_json::from_slice::<DiagnoseReq>(&body)
        .ok()
        .and_then(|r| r.note);
    match crate::ai::diagnose(state, &id, "manual request", note).await {
        Ok(report) => axum::Json(report).into_response(),
        Err(e) => err(e),
    }
}

async fn ai_incidents(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    axum::Json(crate::ai::list_incidents(&state.cfg, &id)).into_response()
}

#[derive(serde::Serialize)]
struct SftpInfo {
    username: String,
    password: String,
    port: u16,
}

async fn sftp_info(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if !state.servers.contains_key(&id) {
        return err(anyhow::anyhow!("unknown server {id}"));
    }
    let info = SftpInfo {
        username: format!("srv.{id}"),
        password: state.sftp_password(&id),
        port: sftp_port(&state),
    };
    axum::Json(info).into_response()
}

async fn sftp_reset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if !state.servers.contains_key(&id) {
        return err(anyhow::anyhow!("unknown server {id}"));
    }
    let info = SftpInfo {
        username: format!("srv.{id}"),
        password: state.reset_sftp_password(&id),
        port: sftp_port(&state),
    };
    axum::Json(info).into_response()
}

async fn server_stats(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match crate::docker::stats(state, &id).await {
        Ok(st) => axum::Json(st).into_response(),
        Err(e) => err(e),
    }
}

fn sftp_port(state: &AppState) -> u16 {
    state
        .cfg
        .sftp
        .bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2022)
}

fn err(e: anyhow::Error) -> Response {
    tracing::debug!(error = %e, "api error");
    (
        StatusCode::BAD_REQUEST,
        serde_json::json!({"error": e.to_string()}).to_string(),
    )
        .into_response()
}

// keep `delete` import used even if routes change
#[allow(unused_imports)]
use delete as _delete_marker;


// ── schedules ────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewSchedule {
    name: String,
    cron: String,
    action: String,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

fn schedule_json(t: &crate::scheduler::Schedule) -> serde_json::Value {
    crate::scheduler::with_next_run(serde_json::to_value(t).unwrap_or_default(), t)
}

async fn schedules_list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let sched = crate::scheduler::Scheduler { app: state };
    let out: Vec<serde_json::Value> =
        sched.list(&id).iter().map(schedule_json).collect();
    axum::Json(out).into_response()
}

async fn schedules_add(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Json(req): axum::Json<NewSchedule>,
) -> Response {
    if !state.servers.contains_key(&id) {
        return err(anyhow::anyhow!("unknown server {id}"));
    }
    let sched = crate::scheduler::Scheduler { app: state };
    let task = crate::scheduler::Schedule {
        id: String::new(),
        name: req.name,
        cron: req.cron,
        action: req.action,
        payload: req.payload.filter(|p| !p.is_empty()),
        enabled: req.enabled,
        last_fired: None,
        last_result: None,
    };
    match sched.add(&id, task) {
        Ok(t) => axum::Json(schedule_json(&t)).into_response(),
        Err(e) => err(e),
    }
}

#[derive(serde::Deserialize)]
struct UpdatePath {
    id: String,
    tid: String,
}

async fn schedules_update(
    State(state): State<Arc<AppState>>,
    Path(p): Path<UpdatePath>,
    axum::Json(req): axum::Json<crate::scheduler::SchedulePatch>,
) -> Response {
    let sched = crate::scheduler::Scheduler { app: state };
    match sched.update(&p.id, &p.tid, req) {
        Ok(t) => axum::Json(schedule_json(&t)).into_response(),
        Err(e) => err(e),
    }
}

async fn schedules_delete(
    State(state): State<Arc<AppState>>,
    Path(p): Path<UpdatePath>,
) -> Response {
    let sched = crate::scheduler::Scheduler { app: state };
    match sched.delete(&p.id, &p.tid) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(e),
    }
}


async fn rerun_script(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let rt = match state.get(&id) {
        Ok(r) => r,
        Err(e) => return err(e),
    };
    let Some(stored) = crate::installer::load_script(&state.cfg, &id) else {
        return err(anyhow::anyhow!("this server has no stored install script"));
    };
    match crate::installer::start_script_install(state, rt, stored.script, stored.image) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => err(e),
    }
}
