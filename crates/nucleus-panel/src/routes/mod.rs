use crate::auth::Sessions;
use crate::config::Config;
use crate::daemon::DaemonClient;
use crate::db::Db;
use crate::models::{EggRow, Node, ServerRow, User};
use anyhow::Result;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use std::sync::Arc;

pub mod admin;
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
    let static_dir = app.cfg.static_dir.clone();

    axum::Router::new()
        .route("/", get(pages::dashboard))
        .route("/login", get(pages::login_get).post(pages::login_post))
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
        .route(
            "/servers/{id}/backups/download",
            get(proxy::backup_download),
        )
        .route("/servers/{id}/ai/diagnose", post(proxy::ai_diagnose))
        .route(
            "/admin/nodes",
            get(admin::nodes_page).post(admin::nodes_add),
        )
        .route(
            "/admin/nodes/{id}/edit",
            post(admin::nodes_edit),
        )
        .route(
            "/admin/eggs",
            get(admin::eggs_page).post(admin::eggs_import),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(app.clone())
        .nest_service("/static", tower_http::services::ServeDir::new(static_dir))
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

pub fn get_server(app: &App, id: &str) -> Option<ServerRow> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT id, name, node_id, egg_slug, image, startup, env_json,
                          ports_json, mem_mb, cpu, stop_command, accept_eula, owner_id
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
        stop_command: r.get(10)?,
        accept_eula: r.get::<_, i64>(11)? != 0,
        owner_id: r.get(12)?,
    })
}

pub fn list_servers(app: &App) -> Vec<ServerRow> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT id, name, node_id, egg_slug, image, startup, env_json,
                          ports_json, mem_mb, cpu, stop_command, accept_eula, owner_id
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
