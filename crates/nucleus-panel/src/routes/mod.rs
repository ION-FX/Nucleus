use crate::auth::Sessions;
use crate::config::Config;
use crate::daemon::DaemonClient;
use crate::db::Db;
use crate::models::{EggRow, Node, ServerRow, User};
use anyhow::Result;
use axum::extract::{Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use std::sync::Arc;

pub mod admin;
pub mod api;
pub mod pages;
pub mod proxy;

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub http: reqwest::Client,
}

impl App {
    /// True until the first account exists; afterwards registration is closed.
    pub fn needs_bootstrap(&self) -> Result<bool> {
        self.db.with(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
            Ok(n == 0)
        })
    }
}

pub type SharedApp = Arc<App>;

pub fn router(app: SharedApp) -> axum::Router {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/", get(pages::dashboard))
        .route("/login", get(pages::login_get).post(pages::login_post))
        .route("/login/2fa", get(pages::login_2fa_get).post(pages::login_2fa_post))
        .route(
            "/register",
            get(pages::register_get).post(pages::register_post),
        )
        .route("/logout", post(pages::logout))
        .route(
            "/servers/new",
            get(pages::server_new).post(pages::server_create),
        )
        .route("/servers/{id}", get(pages::server_console))
        .route("/servers/{id}/stats", get(proxy::server_stats))
        .route("/servers/{id}/power", post(proxy::power))
        .route("/servers/{id}/delete", post(proxy::delete_server))
        .route("/servers/{id}/console", get(pages::console_redirect))
        .route("/servers/{id}/ws", get(proxy::ws_relay))
        .route("/servers/{id}/startup", get(pages::startup_page).post(pages::startup_save))
        .route("/servers/{id}/network", get(pages::network_page).post(pages::network_save))
        .route("/servers/{id}/settings", get(pages::settings_page).post(pages::settings_save))
        .route("/servers/{id}/ai", get(pages::ai_page))
        .route("/servers/{id}/schedules", get(pages::schedules_page))
        .route("/servers/{id}/schedules/add", post(proxy::schedule_add))
        .route("/servers/{id}/schedules/toggle", post(proxy::schedule_toggle))
        .route("/servers/{id}/schedules/delete", post(proxy::schedule_delete))
        .route("/servers/{id}/modpacks", get(pages::modpacks_page))
        .route("/servers/{id}/backups", get(pages::backups_page))
        .route("/servers/{id}/files", get(pages::files_page))
        .route(
            "/servers/{id}/files/edit",
            get(pages::file_edit_page).post(pages::file_edit_save),
        )
        .route("/servers/{id}/files/download", get(proxy::file_download))
        .route("/servers/{id}/files/mkdir", post(proxy::files_mkdir))
        .route("/servers/{id}/files/fetch", post(proxy::files_fetch))
        .route("/servers/{id}/sftp/reset", post(proxy::sftp_reset))
        .route("/servers/{id}/files/delete", post(proxy::files_delete))
        .route("/servers/{id}/files/upload", post(proxy::files_upload))
        .route("/servers/{id}/install", post(proxy::install_pack))
        .route("/servers/{id}/install/script", post(proxy::install_script_rerun))
        .route("/servers/{id}/backups/create", post(proxy::backup_create))
        .route("/servers/{id}/backups/delete", post(proxy::backup_delete))
        .route("/servers/{id}/backups/restore", post(proxy::backup_restore))
        .route(
            "/servers/{id}/backups/download",
            get(proxy::backup_download),
        )
        .route("/servers/{id}/ai/diagnose", post(proxy::ai_diagnose))
        .route("/admin", get(admin::dashboard_page))
        .route("/admin/stats.json", get(admin::dashboard_stats))
        .route("/admin/nodes", get(admin::nodes_page).post(admin::nodes_add))
        .route(
            "/admin/nodes/{id}/edit",
            post(admin::nodes_edit),
        )
        .route("/admin/users", get(admin::users_page).post(admin::users_create))
        .route("/admin/users/{id}/reset", post(admin::users_reset))
        .route("/admin/users/{id}/role", post(admin::users_role_toggle))
        .route("/admin/users/{id}/delete", post(admin::users_delete))
        .route("/admin/activity", get(admin::activity_page))
        .route("/admin/activity/export", get(admin::activity_export))
        .route("/admin/defaults", get(admin::defaults_page).post(admin::defaults_save))
        .route("/admin/update", get(admin::update_page).post(admin::update_perform))
        .route("/account", get(pages::account_page).post(pages::account_password))
        .route("/account/apikeys", post(pages::apikey_create))
        .route("/account/apikeys/delete", post(pages::apikey_delete))
        .route("/account/2fa/enable", post(pages::totp_enable))
        .route("/account/2fa/setup", get(pages::totp_setup_page))
        .route("/account/2fa/confirm", post(pages::totp_confirm))
        .route("/account/2fa/disable", post(pages::totp_disable))
        .route("/admin/invites", get(admin::invites_page).post(admin::invites_create))
        .route("/admin/invites/{token}/delete", post(admin::invites_revoke))
        .route("/servers/{id}/access", get(pages::server_access))
        .route("/servers/{id}/access/add", post(pages::access_add))
        .route("/servers/{id}/access/remove", post(pages::access_remove))
        .route("/servers/{id}/transfer", post(proxy::transfer_server))
        .route("/servers/{id}/schedules/run", post(proxy::schedule_run))
        .route("/servers/{id}/mods/search", get(proxy::mods_search))
        .route("/servers/{id}/mods/versions", get(proxy::mods_versions))
        .route("/servers/{id}/mods/install", post(proxy::mods_install))
        .route("/servers/{id}/files/rename", post(proxy::files_rename))
        .route("/servers/{id}/files/move", post(proxy::files_move))
        .route("/servers/{id}/files/archive", post(proxy::files_archive))
        .route("/servers/{id}/files/extract", post(proxy::files_extract))
        .route(
            "/admin/eggs",
            get(admin::eggs_page).post(admin::eggs_import),
        )
        .route("/static/{*path}", get(static_asset))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(app.clone())
        .nest("/api/v1", api::router(app.clone()))
}

// ---------- static assets (embedded, disk override for dev) ----------

#[derive(rust_embed::RustEmbed)]
#[folder = "static"]
struct StaticAssets;

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Compile-time timestamp of this binary; on-disk static assets are only
/// served when newer, so a stale static/ dir can't shadow embedded ones.
fn build_epoch() -> u64 {
    option_env!("NUCLEUS_BUILD_EPOCH")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

fn asset_etag(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:x}-{}\"", h.finish(), bytes.len())
}

fn asset_response(
    headers: &axum::http::HeaderMap,
    path: &str,
    bytes: impl Into<axum::body::Bytes>,
) -> Response {
    use axum::http::header;
    let bytes: axum::body::Bytes = bytes.into();
    let etag = asset_etag(&bytes);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
    {
        return (
            axum::http::StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, mime_for(path).to_string()),
            (header::ETAG, etag),
            // Revalidate on every load: themes ship via CSS and a stale
            // stylesheet renders half-themed pages.
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        bytes,
    )
        .into_response()
}

async fn static_asset(
    State(app): State<SharedApp>,
    AxumPath(path): AxumPath<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Embedded copy first: the binary must be self-contained and upgrades
    // must win over leftovers from an old checkout or release archive.
    let embedded = StaticAssets::get(&path);
    // Disk override for dev — only while the file is NEWER than this binary
    // AND actually differs from the embedded copy (a stale static/ dir with
    // byte-identical or older files must never shadow the shipped asset).
    let disk = app.cfg.static_dir.join(&path);
    if let Ok(bytes) = tokio::fs::read(&disk).await {
        let differs = embedded
            .as_ref()
            .map(|e| e.data.as_ref() != bytes.as_slice())
            .unwrap_or(true);
        let newer = tokio::fs::metadata(&disk)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() >= build_epoch())
            .unwrap_or(false);
        if newer && differs {
            return asset_response(&headers, &path, bytes);
        }
    }
    match embedded {
        Some(asset) => asset_response(&headers, &path, asset.data.into_owned()),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- guards ----------

pub fn user_guard(app: &App, headers: &HeaderMap) -> Result<User, Response> {
    match Sessions::user_for(&app.db, headers) {
        Some(u) => Ok(u),
        None => Err(Redirect::to("/login").into_response()),
    }
}

pub fn admin_guard(app: &App, headers: &HeaderMap) -> Result<User, Response> {
    let u = user_guard(app, headers)?;
    if u.role != "admin" {
        return Err((
            axum::http::StatusCode::FORBIDDEN,
            "admin access required".to_string(),
        )
            .into_response());
    }
    Ok(u)
}

// ---------- db helpers ----------

pub fn list_nodes(app: &App) -> Vec<Node> {
    app.db
        .with(|c| {
            let mut stmt =
                c.prepare("SELECT id, name, url, token, alias FROM nodes ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Node {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        url: r.get(2)?,
                        token: r.get(3)?,
                        alias: r.get(4)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
}

pub fn get_node(app: &App, id: &str) -> Option<Node> {
    list_nodes(app).into_iter().find(|n| n.id == id)
}

pub fn daemon_for(app: &App, node_id: &str) -> Result<DaemonClient> {
    let node = get_node(app, node_id).ok_or_else(|| anyhow::anyhow!("node not found"))?;
    Ok(DaemonClient::new(app.http.clone(), &node))
}

/// If the node lost this server (crashed mid-write corrupted its registry,
/// or the registry was wiped), re-register it from the panel DB — the row
/// carries the full spec, and data files on the node survive. No-op when the
/// node already knows the server. Best-effort: callers proceed either way so
/// the real operation surfaces the real error.
pub async fn heal_node_server(app: &App, srv: &ServerRow) {
    let Ok(d) = daemon_for(app, &srv.node_id) else {
        return;
    };
    if let Err(e) = d.status(&srv.id).await {
        if !e.to_string().contains("unknown server") {
            return;
        }
        let spec = nucleus_core::CreateServerRequest {
            id: srv.id.clone(),
            name: srv.name.clone(),
            image: srv.image.clone(),
            startup: srv.startup.clone(),
            env: serde_json::from_str(&srv.env_json).unwrap_or_default(),
            ports: serde_json::from_str(&srv.ports_json).unwrap_or_default(),
            limits: nucleus_core::Limits {
                mem_mb: srv.mem_mb,
                cpu_cores: srv.cpu,
                disk_mb: srv.disk_mb,
                pids_limit: srv.pids_limit,
            },
            stop_command: srv.stop_command.clone(),
            accept_eula: srv.accept_eula,
            install_script: None,
            installer_image: None,
        };
        let _ = d.create_server(&spec).await;
    }
}

pub fn get_server(app: &App, id: &str) -> Option<ServerRow> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT id, name, node_id, egg_slug, image, startup, env_json,
                          ports_json, mem_mb, cpu, disk_mb, pids_limit, tags,
                          stop_command, accept_eula, owner_id
                   FROM servers WHERE id = ?1"#,
            )?;
            let row = stmt.query_map([id], map_server_row)?.next().transpose()?;
            Ok(row)
        })
        .ok()
        .flatten()
}

fn map_server_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ServerRow> {
    Ok(ServerRow {
        id: r.get(0)?,
        name: r.get(1)?,
        node_id: r.get(2)?,
        egg_slug: r.get(3)?,
        image: r.get(4)?,
        startup: r.get(5)?,
        env_json: r.get(6)?,
        ports_json: r.get(7)?,
        mem_mb: r.get::<_, i64>(8)? as u64,
        cpu: r.get(9)?,
        disk_mb: r.get::<_, i64>(10)? as u64,
        pids_limit: r.get(11)?,
        tags: r.get(12)?,
        stop_command: r.get(13)?,
        accept_eula: r.get::<_, i64>(14)? != 0,
        owner_id: r.get(15)?,
    })
}

pub fn list_servers(app: &App) -> Vec<ServerRow> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT id, name, node_id, egg_slug, image, startup, env_json,
                          ports_json, mem_mb, cpu, disk_mb, pids_limit, tags,
                          stop_command, accept_eula, owner_id
                   FROM servers ORDER BY created_at DESC"#,
            )?;
            let rows = stmt
                .query_map([], map_server_row)?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
}

pub fn list_eggs(app: &App) -> Vec<EggRow> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare("SELECT slug, name, json FROM eggs ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| {
                    let slug: String = r.get(0)?;
                    let name: String = r.get(1)?;
                    let json: String = r.get(2)?;
                    Ok((slug, name, json))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(slug, name, json)| {
                    serde_json::from_str::<nucleus_core::Egg>(&json)
                        .ok()
                        .map(|egg| EggRow { slug, name, egg })
                })
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
}
