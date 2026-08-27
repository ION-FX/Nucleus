use super::*;
use crate::auth::{self, Sessions};
use qrcode::QrCode;
use qrcode::render::svg;
use anyhow::Result;
use askama::Template;
use axum::extract::{Form, Multipart, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};

pub(crate) fn page<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e}"),
        )
            .into_response(),
    }
}

pub(crate) fn val(form: &[(String, String)], key: &str) -> String {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn redirect_with_cookie(to: &str, cookie: String) -> Response {
    let mut resp = Redirect::to(to).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    resp
}

pub fn nav_ctx(app: &App, headers: &HeaderMap) -> (String, bool) {
    match Sessions::user_for(&app.db, headers) {
        Some(u) => (u.email.clone(), u.role == "admin"),
        None => (String::new(), false),
    }
}

fn human_size(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct BackupInfo {
    pub id: String,
    pub size: u64,
    pub created_at: i64,
}

// ---------- auth ----------

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTmpl {
    pub message: String,
    pub error: String,
    pub allow_register: bool,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn login_get(State(app): State<SharedApp>) -> Response {
    let allow = app.needs_bootstrap().unwrap_or(false);
    let mut t = LoginTmpl {
        message: String::new(),
        error: String::new(),
        allow_register: allow,
        user_email: String::new(),
        is_admin: false,
    };
    if allow {
        t.message = "Welcome! Create the first administrator account to get started.".into();
    }
    page(&t)
}

fn login_error(allow_register: bool) -> Response {
    page(&LoginTmpl {
        message: String::new(),
        error: "Invalid email or password.".into(),
        allow_register,
        user_email: String::new(),
        is_admin: false,
    })
}

pub async fn login_post(
    State(app): State<SharedApp>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let email = val(&form, "email").to_lowercase();
    let password = val(&form, "password");
    let allow = app.needs_bootstrap().unwrap_or(false);

    let row: Result<Option<(i64, String)>> = app.db.with(|c| {
        Ok(c.query_row(
            "SELECT id, password_hash FROM users WHERE email = ?1",
            [&email],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok())
    });
    let Ok(Some((id, hash))) = row else {
        return login_error(allow);
    };
    if !auth::verify_password(&password, &hash) {
        return login_error(allow);
    }
    let totp_on: bool = app
        .db
        .with(|c| Ok(c.query_row("SELECT COALESCE(totp_enabled,0) FROM users WHERE id=?1", rusqlite::params![id], |r| r.get::<_, i64>(0))?))
        .unwrap_or(0) != 0;
    if totp_on {
        let ptoken = auth::create_pending_2fa(&app.db, id);
        return redirect_with_cookie("/login/2fa", auth::Sessions::pending_cookie(&ptoken));
    }
    match Sessions::create(&app.db, id) {
        Ok(token) => redirect_with_cookie("/", Sessions::session_cookie(&token)),
        Err(_) => login_error(allow),
    }
}

#[derive(Template)]
#[template(path = "register.html")]
pub struct RegisterTmpl {
    pub error: String,
    pub invite: String,
    pub invited_email: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn register_get(
    State(app): State<SharedApp>,
    Query(q): Query<InviteQuery>,
) -> Response {
    if !app.needs_bootstrap().unwrap_or(true) && q.invite.is_none() {
        return Redirect::to("/login").into_response();
    }
    let invited_email: String = q
        .invite
        .as_ref()
        .and_then(|tok| {
            app.db
                .with(|c| Ok(c.query_row("SELECT email FROM invites WHERE token=?1 AND used_at IS NULL", rusqlite::params![tok], |r| r.get(0)).ok()))
            .ok().flatten()
        })
        .unwrap_or_default();
    page(&RegisterTmpl {
        error: String::new(),
        invite: q.invite.clone().unwrap_or_default(),
        invited_email,
        user_email: String::new(),
        is_admin: false,
    })
}

#[derive(serde::Deserialize)]
pub struct InviteQuery {
    pub invite: Option<String>,
}

pub async fn register_post(
    State(app): State<SharedApp>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let email = val(&form, "email").trim().to_lowercase();
    let password = val(&form, "password");
    let invite = val(&form, "invite");

    let role = if !invite.is_empty() {
        let row: Option<(String, String)> = app
            .db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT email, role FROM invites WHERE token=?1 AND used_at IS NULL",
                    rusqlite::params![invite],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ).ok())
            })
            .ok().flatten();
        match row {
            Some((inv_email, inv_role)) if inv_email == email => inv_role,
            _ => {
                return page(&RegisterTmpl {
                    error: "Invite is invalid or already used for that email.".into(),
                    user_email: String::new(),
                    is_admin: false,
                    invite: String::new(),
                    invited_email: String::new(),
                })
            }
        }
    } else {
        if !app.needs_bootstrap().unwrap_or(true) {
            return Redirect::to("/login").into_response();
        }
        "admin".to_string()
    };

    if email.is_empty() || password.len() < 8 {
        return page(&RegisterTmpl {
            error: "Email required and password must be at least 8 characters.".into(),
            user_email: String::new(),
            is_admin: false,
            invite: String::new(),
            invited_email: String::new(),
        });
    }
    let hash = match auth::hash_password(&password) {
        Ok(h) => h,
        Err(_) => {
            return page(&RegisterTmpl {
                error: "Failed to hash password.".into(),
                user_email: String::new(),
                is_admin: false,
                invite: String::new(),
                invited_email: String::new(),
            })
        }
    };
    let res = app.db.with(|c| {
        c.execute(
            "INSERT INTO users (email, password_hash, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![email, hash, role, chrono::Utc::now().timestamp()],
        )?;
        Ok(c.last_insert_rowid())
    });
    match res {
        Ok(id) => {
            if !invite.is_empty() {
                let _ = app.db.with(|c| {
                    c.execute(
                        "UPDATE invites SET used_at=?1 WHERE token=?2",
                        rusqlite::params![chrono::Utc::now().timestamp(), invite],
                    )?;
                    Ok(())
                });
            }
            let token = Sessions::create(&app.db, id).unwrap_or_default();
            redirect_with_cookie("/", Sessions::session_cookie(&token))
        }
        Err(_) => page(&RegisterTmpl {
            error: "Could not create account (maybe already exists).".into(),
            user_email: String::new(),
            is_admin: false,
            invite: String::new(),
            invited_email: String::new(),
        }),
    }
}

pub async fn logout(State(app): State<SharedApp>, headers: HeaderMap) -> Response {
    Sessions::destroy(&app.db, &headers);
    redirect_with_cookie("/login", Sessions::clear_cookie())
}

// ---------- dashboard ----------

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTmpl {
    pub servers: Vec<ServerCard>,
    pub all_tags: Vec<String>,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct ServerCard {
    pub id: String,
    pub name: String,
    pub node_name: String,
    pub running: bool,
    pub status_class: String,
    pub status_text: String,
    pub tags: String,
}

pub async fn dashboard(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (user_email, is_admin) = nav_ctx(&app, &headers);
    if user_email.is_empty() {
        return Err(Redirect::to("/login").into_response());
    }

    let rows = list_servers(&app);
    let user = crate::auth::Sessions::user_for(&app.db, &headers);
    let mut cards = Vec::new();
    for s in rows {
        // Non-admins only see servers they own or are a member of.
        if let Some(u) = &user {
            if u.role != "admin" && s.owner_id != Some(u.id) {
                let member = app
                    .db
                    .with(|c| {
                        let mut stmt = c
                            .prepare("SELECT 1 FROM user_servers WHERE user_id=?1 AND server_id=?2")?;
                        let mut r = stmt.query(rusqlite::params![u.id, s.id])?;
                        Ok(r.next()?.is_some())
                    })
                    .unwrap_or(false);
                if !member {
                    continue;
                }
            }
        }
        let node = get_node(&app, &s.node_id);
        let node_name = node
            .as_ref()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| "?".into());
        let st = match &node {
            Some(n) => {
                let client = DaemonClient::new(app.http.clone(), n);
                match tokio::time::timeout(std::time::Duration::from_secs(4), client.status(&s.id))
                    .await
                {
                    Ok(Ok(st)) => Some(st),
                    _ => None,
                }
            }
            None => None,
        };
        let (running, status_class, status_text) = match st {
            Some(st) if st.running => (true, "green".into(), "Running".into()),
            Some(st) => (
                false,
                "red".into(),
                match st.exit_code {
                    Some(0) => "Stopped".to_string(),
                    Some(c) => format!("Exited ({c})"),
                    None => "Stopped".to_string(),
                },
            ),
            None => (false, "grey".into(), "Offline".into()),
        };
        cards.push(ServerCard {
            id: s.id.clone(),
            name: s.name.clone(),
            node_name,
            running,
            status_class,
            status_text,
            tags: s.tags.clone(),
        });
    }
    let mut tag_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for c in &cards {
        for t in c.tags.split(',') {
            let t = t.trim();
            if !t.is_empty() {
                tag_set.insert(t.to_string());
            }
        }
    }
    Ok(page(&DashboardTmpl {
        servers: cards,
        all_tags: tag_set.into_iter().collect(),
        user_email,
        is_admin,
    }))
}

// ---------- server create ----------

#[derive(Template)]
#[template(path = "server_new.html")]
pub struct ServerNewTmpl {
    pub nodes: Vec<NodeOpt>,
    pub eggs: Vec<EggOpt>,
    pub eggs_full_json: String,
    pub form_name: String,
    pub error: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct NodeOpt {
    pub id: String,
    pub name: String,
}

pub struct EggOpt {
    pub slug: String,
    pub name: String,
    pub images_json: String,
    pub startup_attr: String,
}

fn egg_opts(app: &App) -> (Vec<EggOpt>, String) {
    let eggs = list_eggs(app);
    let full: Vec<serde_json::Value> = eggs
        .iter()
        .map(|e| {
            serde_json::json!({
                "slug": e.slug,
                "name": e.name,
                "images": e.egg.docker_images,
                "startup": e.egg.startup,
                "vars": e.egg.variables.iter().map(|v| serde_json::json!({
                    "env": v.env_variable,
                    "name": v.name,
                    "default": v.default_value,
                    "editable": v.user_editable,
                    "required": v.required,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let opts = eggs
        .iter()
        .map(|e| EggOpt {
            slug: e.slug.clone(),
            name: e.name.clone(),
            images_json: serde_json::to_string(&e.egg.docker_images)
                .unwrap_or_else(|_| "[]".into()),
            startup_attr: html_escape(&e.egg.startup),
        })
        .collect();
    (
        opts,
        serde_json::to_string(&full).unwrap_or_else(|_| "[]".into()),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn server_new(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    admin_guard(&app, &headers)?;
    let (eggs, full) = egg_opts(&app);
    let nodes = list_nodes(&app)
        .into_iter()
        .map(|n| NodeOpt {
            id: n.id,
            name: n.name,
        })
        .collect();
    Ok(page(&ServerNewTmpl {
        nodes,
        eggs,
        eggs_full_json: full,
        form_name: String::new(),
        error: String::new(),
        user_email: nav_ctx(&app, &headers).0,
        is_admin: true,
    }))
}

pub async fn server_create(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Result<Response, Response> {
    let user = admin_guard(&app, &headers)?;

    // Collect text fields + optional modpack file from the multipart form.
    let mut form: Vec<(String, String)> = Vec::new();
    let mut pack: Option<(String, Vec<u8>)> = None;
    while let Some(field) = mp.next_field().await.ok().flatten() {
        let fname = field.file_name().map(str::to_owned);
        match field.name().unwrap_or("").to_string() {
            name if name == "modpack" => {
                if let Ok(b) = field.bytes().await {
                    if !b.is_empty() {
                        pack = Some((fname.unwrap_or_else(|| "pack.zip".into()), b.to_vec()));
                    }
                }
            }
            name => {
                if let Ok(v) = field.text().await {
                    form.push((name, v));
                }
            }
        }
    }

    let fail = |msg: &str, name: &str| -> Response {
        let (eggs, full) = egg_opts(&app);
        let nodes = list_nodes(&app)
            .into_iter()
            .map(|n| NodeOpt {
                id: n.id,
                name: n.name,
            })
            .collect();
        page(&ServerNewTmpl {
            nodes,
            eggs,
            eggs_full_json: full,
            form_name: name.to_string(),
            error: msg.to_string(),
            user_email: user.email.clone(),
            is_admin: true,
        })
        .into_response()
    };

    let raw_name = val(&form, "name").trim().to_string();
    // If a modpack was attached, let the daemon inspect it and drive the
    // image/startup choice — the pack is the source of truth in this mode.
    if let Some((_, data)) = &pack {
        let daemon0 = match daemon_for(&app, &val(&form, "node_id")) {
            Ok(d) => d,
            Err(_) => return Err(fail("Unknown node.", &raw_name)),
        };
        match daemon0.inspect_pack(data.clone()).await {
            Ok(insight) => {
                // Hidden form sections still submit empty values; drop them so
                // the recommendation below becomes the single source of truth.
                form.retain(|(k, v)| {
                    !((k == "image" || k == "startup_raw" || k == "egg_slug") && v.is_empty())
                });
                if let Some(img) = insight["recommendedImage"].as_str() {
                    form.push(("image".into(), img.to_string()));
                }
                if let Some(st) = insight["recommendedStartup"].as_str() {
                    form.push(("startup_raw".into(), st.to_string()));
                }
            }
            Err(e) => return Err(fail(&format!("Could not read that pack: {e:#}"), &raw_name)),
        }
    }

    let name = raw_name;
    if name.is_empty() {
        return Err(fail("Name is required.", &name));
    }
    let node_id = val(&form, "node_id");
    let daemon = match daemon_for(&app, &node_id) {
        Ok(d) => d,
        Err(_) => return Err(fail("Unknown node.", &name)),
    };

    // Resolve image + startup from egg or raw fields.
    let egg_slug = val(&form, "egg_slug");
    let mut env = std::collections::BTreeMap::new();
    let (image, startup_tpl, stop_command) = if egg_slug == "custom" || egg_slug.is_empty() {
        (
            val(&form, "image").trim().to_string(),
            val(&form, "startup_raw"),
            opt_val(&form, "stop_command"),
        )
    } else {
        let Some(egg_row) = list_eggs(&app).into_iter().find(|e| e.slug == egg_slug) else {
            return Err(fail("Unknown egg selected.", &name));
        };
        for v in &egg_row.egg.variables {
            env.insert(v.env_variable.clone(), v.default_value.clone());
        }
        for (k, v) in &form {
            if let Some(envk) = k.strip_prefix("var_") {
                if !v.is_empty() {
                    env.insert(envk.to_string(), v.clone());
                }
            }
        }
        // Standard variables every Pterodactyl-style template expects.
        env.entry("SERVER_MEMORY".to_string())
            .or_insert_with(|| val(&form, "mem_mb"));
        env.entry("SERVER_PORT".to_string())
            .or_insert_with(|| "25565".to_string());
        let stop = egg_row
            .egg
            .stop_command
            .clone()
            .or_else(|| opt_val(&form, "stop_command"));
        (
            egg_row.egg.docker_images[0].clone(),
            egg_row.egg.startup.clone(),
            stop,
        )
    };

    if image.is_empty() {
        return Err(fail(
            "Couldn't determine a Docker image from this pack (no loader info?). Use Custom mode and choose an image manually.",
            &name,
        ));
    }

    let mut ports = Vec::new();
    for line in val(&form, "ports").lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() != 2 {
            return Err(fail(
                &format!("Bad port mapping '{line}' (want host:container/proto)"),
                &name,
            ));
        }
        let (proto, hp_cp) = match parts[1].split_once('/') {
            Some((p, pr)) => (pr.to_string(), p),
            None => ("tcp".to_string(), parts[1]),
        };
        let Ok(host) = hp_cp.parse::<u16>() else {
            return Err(fail(&format!("Bad host port in '{line}'"), &name));
        };
        let Ok(container) = proto_parse_container(&parts[0]) else {
            return Err(fail(&format!("Bad container port in '{line}'"), &name));
        };
        ports.push(nucleus_core::PortMapping {
            host,
            container,
            proto,
        });
    }

    let mem_mb = val(&form, "mem_mb").parse::<u64>().unwrap_or(2048).max(128);
    let cpu = val(&form, "cpu").parse::<f64>().unwrap_or(2.0).max(0.25);
    let disk_mb = val(&form, "disk_mb").parse::<u64>().unwrap_or(0);
    let pids_limit = val(&form, "pids_limit").parse::<i64>().unwrap_or(0);

    // Pterodactyl-style built-ins — always derived from server config so
    // templates like `-Xmx{{SERVER_MEMORY}}M` never render empty.
    env.insert("SERVER_MEMORY".to_string(), mem_mb.to_string());
    env.insert("SERVER_IP".to_string(), "0.0.0.0".to_string());
    if let Some(p) = ports.first() {
        env.insert("SERVER_PORT".to_string(), p.container.to_string());
    }

    let rendered = nucleus_core::render_startup(&startup_tpl, &env);

    // Attach the egg's install script (sidecar-executed on the node).
    // Stale egg imports may lack the installer image; egg scripts are written
    // for Pterodactyl's installer images, so never let them fall back to the
    // game server image (wine yolks break steamcmd).
    const DEFAULT_INSTALLER: &str = "ghcr.io/ptero-eggs/installers:debian";
    let (install_script, installer_image) = if !egg_slug.is_empty() && egg_slug != "custom" {
        list_eggs(&app)
            .into_iter()
            .find(|e| e.slug == egg_slug)
            .map(|e| {
                let img = Some(
                    e.egg
                        .installer_image
                        .clone()
                        .unwrap_or_else(|| DEFAULT_INSTALLER.to_string()),
                );
                (e.egg.install_script.clone(), img)
            })
            .unwrap_or((None, None))
    } else {
        (None, None)
    };

    let spec = nucleus_core::CreateServerRequest {
        id: nucleus_core::new_server_id(),
        name: name.clone(),
        image: image.clone(),
        startup: rendered,
        env,
        ports,
        limits: nucleus_core::Limits {
            mem_mb,
            cpu_cores: cpu,
            disk_mb,
            pids_limit,
        },
        stop_command,
        accept_eula: val(&form, "accept_eula") == "1",
        install_script,
        installer_image,
    };

    let created = daemon
        .create_server(&spec)
        .await
        .map_err(|e| fail(&format!("Node rejected server: {e:#}"), &name))?;

    app.db
        .with(|c| {
            c.execute(
                r#"INSERT INTO servers (id, name, node_id, egg_slug, image, startup, env_json,
                       ports_json, mem_mb, cpu, disk_mb, pids_limit, tags,
                       stop_command, accept_eula, owner_id, created_at)
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"#,
                rusqlite::params![
                    spec.id,
                    spec.name,
                    node_id.clone(),
                    if egg_slug == "custom" {
                        None
                    } else {
                        Some(egg_slug.as_str())
                    },
                    spec.image,
                    spec.startup,
                    serde_json::to_string(&spec.env).unwrap_or_default(),
                    serde_json::to_string(&spec.ports).unwrap_or_default(),
                    spec.limits.mem_mb as i64,
                    spec.limits.cpu_cores,
                    spec.limits.disk_mb as i64,
                    spec.limits.pids_limit,
                    val(&form, "tags").trim().to_string(),
                    spec.stop_command,
                    spec.accept_eula as i64,
                    user.id,
                    chrono::Utc::now().timestamp()
                ],
            )?;
            Ok(())
        })
        .map_err(|e| fail(&format!("DB insert failed: {e}"), &name))?;

    tracing::info!(server = %created.id, "server created");

    // Kick off the modpack install job on the node if a pack was attached.
    if let Some((fname, data)) = pack {
        let size = data.len();
        match daemon.upload_pack(&created.id, &fname, data).await {
            Ok(()) => {
                tracing::info!(server = %created.id, %fname, size, "pack install started");
            }
            Err(e) => {
                return Ok(Redirect::to(&format!(
                    "/servers/{}?error={}",
                    created.id,
                    urlencoding::encode(&format!(
                        "Server created, but the modpack upload failed: {e:#}"
                    ))
                ))
                .into_response())
            }
        }
    }

    Ok(Redirect::to(&format!("/servers/{}", created.id)).into_response())
}

fn proto_parse_container(s: &str) -> Result<u16> {
    s.parse::<u16>().map_err(Into::into)
}

fn opt_val(form: &[(String, String)], key: &str) -> Option<String> {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------- server shell pages ----------

#[derive(Clone)]
pub struct ReportView {
    pub finished_at: String,
    pub summary: String,
    pub actions: Vec<String>,
}
// ── Pterodactyl-style server shell ───────────────────────────────────────

#[derive(Clone)]
pub struct ShellCtx {
    pub id: String,
    pub name: String,
    pub image: String,
    pub node_name: String,
    pub running: bool,
    pub status_class: String,
    pub status_text: String,
    pub active: String,
    /// Public hostname players connect to (node alias or daemon host).
    pub addr_host: String,
    /// "host:port" of the primary allocation, empty when none.
    pub addr_full: String,
    /// Port as display string, empty when none.
    pub addr_port_text: String,
    pub can_console: bool,
    pub can_power: bool,
    pub can_files: bool,
    pub can_backups: bool,
    pub can_modpacks: bool,
    pub can_ai: bool,
    pub can_schedules: bool,
    pub can_settings: bool,
    pub can_access: bool,
}

fn tab_flag(active: &str) -> &'static str {
    match active {
        "network" | "startup" | "settings" => "settings",
        "files" => "files",
        "backups" => "backups",
        "modpacks" => "modpacks",
        "ai" => "ai",
        "schedules" => "schedules",
        "access" => "access",
        _ => "console",
    }
}

async fn build_shell(
    app: &App,
    headers: &HeaderMap,
    id: &str,
    active: &str,
) -> Result<(ShellCtx, crate::models::ServerRow, Option<DaemonClient>), Response> {
    let Some(user) = crate::auth::Sessions::user_for(&app.db, headers) else {
        return Err(Redirect::to("/login").into_response());
    };
    let user_email = user.email.clone();
    let Some(srv) = get_server(app, id) else {
        return Err((StatusCode::NOT_FOUND, "no such server").into_response());
    };
    let perms = crate::perms::for_server(&app.db, &user, &srv);
    if !perms.has(tab_flag(active)) {
        return Err((
            StatusCode::FORBIDDEN,
            "403 — you don't have access to this section of the server.",
        )
            .into_response());
    }
    let node = get_node(app, &srv.node_id);
    let daemon = node.as_ref().map(|n| DaemonClient::new(app.http.clone(), n));

    let (running, status_class, status_text) = match &daemon {
        Some(d) => match tokio::time::timeout(
            std::time::Duration::from_secs(4),
            d.status(&srv.id),
        )
        .await
        {
            Ok(Ok(st)) if st.running => {
                (true, "green".into(), "Running".into())
            }
            Ok(Ok(st)) => (
                false,
                "red".into(),
                match st.exit_code {
                    Some(0) | None => "Stopped".to_string(),
                    Some(c) => format!("Exited ({c})"),
                },
            ),
            _ => (false, "grey".into(), "Node offline".into()),
        },
        None => (false, "grey".into(), "Node offline".into()),
    };

    let alias = node
        .as_ref()
        .map(|n| n.alias.trim().to_string())
        .filter(|a| !a.is_empty());
    let addr_host = alias.clone().unwrap_or_else(|| {
        node.as_ref()
            .map(|n| url_host(&n.url))
            .unwrap_or_else(|| "?".into())
    });
    let addr_port = serde_json::from_str::<Vec<nucleus_core::PortMapping>>(&srv.ports_json)
        .unwrap_or_default()
        .first()
        .map(|p| p.host);
    let (addr_full, addr_port_text) = match addr_port {
        Some(p) => (format!("{addr_host}:{p}"), p.to_string()),
        None => (String::new(), String::new()),
    };

    Ok((
        ShellCtx {
            id: srv.id.clone(),
            name: srv.name.clone(),
            image: srv.image.clone(),
            node_name: node.as_ref().map(|n| n.name.clone()).unwrap_or_else(|| "?".into()),
            running,
            status_class,
            status_text,
            active: active.to_string(),
            addr_host,
            addr_full,
            addr_port_text,
            can_console: perms.has("console"),
            can_power: perms.has("power"),
            can_files: perms.has("files"),
            can_backups: perms.has("backups"),
            can_modpacks: perms.has("modpacks"),
            can_ai: perms.has("ai"),
            can_schedules: perms.has("schedules"),
            can_settings: perms.has("settings"),
            can_access: perms.has("access"),
        },
        srv,
        daemon,
    ))
}

async fn shell_guard(app: &App, headers: &HeaderMap, id: &str, active: &str) -> Result<(ShellCtx, crate::models::ServerRow, DaemonClient), Response> {
    let (ctx, srv, daemon) = build_shell(app, headers, id, active).await?;
    let Some(d) = daemon else {
        return Err((StatusCode::BAD_GATEWAY, "node missing").into_response());
    };
    Ok((ctx, srv, d))
}

// console (main view)

#[derive(Template)]
#[template(path = "console.html")]
pub struct ConsoleTmpl2 {
    pub shell: ShellCtx,
    pub ws_path: String,
    pub recent_logs: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn server_console(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, _srv, daemon) = shell_guard(&app, &headers, &id, "console").await?;
    let logs = daemon.logs(&shell.id, 100).await.unwrap_or_default();
    Ok(page(&ConsoleTmpl2 {
        ws_path: format!("/servers/{id}/ws"),
        recent_logs: logs,
        shell,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}

pub async fn console_redirect(
    State(_app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    Redirect::to(&format!("/servers/{id}")).into_response()
}

// backups page

#[derive(Template)]
#[template(path = "backups.html")]
pub struct BackupsTmpl {
    pub shell: ShellCtx,
    pub backups: Vec<BackupView>,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn backups_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, daemon) = shell_guard(&app, &headers, &id, "backups").await?;
    let list = daemon.list_backups(&srv.id).await.unwrap_or_default();
    let backups = list
        .iter()
        .filter_map(|v| serde_json::from_value::<BackupInfo>(v.clone()).ok())
        .map(|b| BackupView {
            id: b.id.clone(),
            size_mb: format!("{:.1}", b.size as f64 / 1024.0 / 1024.0),
            created: fmt_ts(b.created_at),
            download_url: format!("/servers/{}/backups/download?bid={}", srv.id, b.id),
        })
        .collect();
    Ok(page(&BackupsTmpl {
        shell,
        backups,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}

// modpacks page

#[derive(Template)]
#[template(path = "modpacks.html")]
pub struct ModpacksTmpl {
    pub shell: ShellCtx,
    pub install_lines: String,
    pub has_script: bool,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn modpacks_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, daemon) = shell_guard(&app, &headers, &id, "modpacks").await?;
    let install_lines = daemon
        .install_status(&srv.id)
        .await
        .map(|s| s.lines.join("\n"))
        .unwrap_or_default();
    let has_script = srv
        .egg_slug
        .as_deref()
        .and_then(|slug| list_eggs(&app).into_iter().find(|e| e.slug == slug))
        .map(|e| e.egg.install_script.is_some())
        .unwrap_or(false);
    Ok(page(&ModpacksTmpl {
        shell,
        install_lines,
        has_script,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}

// network page

#[derive(Template)]
#[template(path = "network.html")]
pub struct NetworkTmpl {
    pub shell: ShellCtx,
    pub ports_text: String,
    pub message: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn network_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PageQueryMsg>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, _daemon) = shell_guard(&app, &headers, &id, "network").await?;
    let ports: Vec<nucleus_core::PortMapping> =
        serde_json::from_str(&srv.ports_json).unwrap_or_default();
    let ports_text = ports
        .iter()
        .map(|p| format!("{}:{}/{}", p.host, p.container, p.proto))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(page(&NetworkTmpl {
        shell,
        ports_text,
        message: q.msg.clone().unwrap_or_default(),
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}

#[derive(serde::Deserialize)]
pub struct PageQueryMsg {
    #[serde(default)]
    pub msg: Option<String>,
}

fn parse_ports(text: &str) -> Result<Vec<nucleus_core::PortMapping>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() != 2 {
            return Err(format!("'{line}' should be host:container/proto"));
        }
        let (proto, host_part) = match parts[1].split_once('/') {
            Some((p, pr)) => (pr.to_string(), p.to_string()),
            None => ("tcp".to_string(), parts[1].to_string()),
        };
        let host: u16 = host_part.parse().map_err(|_| format!("bad host port in '{line}'"))?;
        let container: u16 = parts[0].parse().map_err(|_| format!("bad container port in '{line}'"))?;
        out.push(nucleus_core::PortMapping { host, container, proto });
    }
    Ok(out)
}

pub async fn network_save(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    let (_u, is_admin) = nav_ctx(&app, &headers);
    if is_admin == false && user_guard(&app, &headers).is_err() {
        return Err(Redirect::to("/login").into_response());
    }
    let ports = parse_ports(&val(&form, "ports")).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("{e} — <a href='/servers/{id}/network'>back</a>"),
        )
            .into_response()
    })?;

    // Stop first: docker needs a recreate to apply port maps.
    let (shell, _srv, daemon) = shell_guard(&app, &headers, &id, "network").await?;
    if shell.running {
        daemon
            .power(&shell.id, nucleus_core::PowerAction::Stop)
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("could not stop server to apply ports: {e:#} — <a href='/servers/{id}/network'>back</a>"),
                )
                    .into_response()
            })?;
    }

    let body = serde_json::json!({ "ports": ports });
    daemon
        .update_config(&shell.id, &body)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("save failed: {e:#} — <a href='/servers/{id}/network'>back</a>"),
            )
                .into_response()
        })?;

    app.db
        .with(|c| {
            c.execute(
                "UPDATE servers SET ports_json=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&ports).unwrap_or_default(), shell.id],
            )?;
            Ok(())
        })
        .ok();

    Ok(Redirect::to(&format!(
        "/servers/{id}/network?msg={}",
        urlencoding::encode("Port mappings saved. Start the server to apply.")
    ))
    .into_response())
}

// startup page

#[derive(Template)]
#[template(path = "startup.html")]
pub struct StartupTmpl {
    pub shell: ShellCtx,
    pub egg_slug: String,
    pub mem_mb: u64,
    pub cpu: f64,
    pub stop_command: String,
    pub startup_escaped: String,
    pub message: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn startup_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PageQueryMsg>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, _daemon) = shell_guard(&app, &headers, &id, "startup").await?;
    let startup_escaped = srv.startup.replace("{{", "&#123;&#123;").replace("}}", "&#125;&#125;");
    Ok(page(&StartupTmpl {
        startup_escaped,
        egg_slug: srv.egg_slug.clone().unwrap_or_else(|| "custom".into()),
        mem_mb: srv.mem_mb,
        cpu: srv.cpu,
        stop_command: srv.stop_command.clone().unwrap_or_default(),
        message: q.msg.clone().unwrap_or_default(),
        shell,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}

pub async fn startup_save(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    admin_guard(&app, &headers)?;
    let (shell, _srv, daemon) = shell_guard(&app, &headers, &id, "startup").await?;

    let image = val(&form, "image").trim().to_string();
    let startup = val(&form, "startup");
    let stop = opt_val(&form, "stop_command");
    let mem_mb = val(&form, "mem_mb").parse::<u64>().unwrap_or(shell_running_mem(&form));
    let cpu = val(&form, "cpu").parse::<f64>().unwrap_or(2.0);
    let disk_mb = val(&form, "disk_mb").parse::<u64>().unwrap_or(0);
    let pids_limit = val(&form, "pids_limit").parse::<i64>().unwrap_or(0);

    if image.is_empty() || startup.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Image and startup are required — <a href='/servers/{id}/startup'>back</a>"),
        )
            .into_response());
    }

    let body = serde_json::json!({
        "image": image,
        "startup": startup,
        "stop_command": stop,
        "limits": {"mem_mb": mem_mb.max(128), "cpu_cores": cpu.max(0.25), "disk_mb": disk_mb, "pids_limit": pids_limit},
    });
    daemon
        .update_config(&shell.id, &body)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("save failed: {e:#} — <a href='/servers/{id}/startup'>back</a>"),
            )
                .into_response()
        })?;

    app.db
        .with(|c| {
            c.execute(
                "UPDATE servers SET image=?1, startup=?2, stop_command=?3, mem_mb=?4, cpu=?5, disk_mb=?6, pids_limit=?7 WHERE id=?8",
                rusqlite::params![image, startup, stop, mem_mb as i64, cpu, disk_mb as i64, pids_limit, shell.id],
            )?;
            Ok(())
        })
        .ok();

    Ok(Redirect::to(&format!(
        "/servers/{id}/startup?msg={}",
        urlencoding::encode("Saved. Restart the server to apply changes.")
    ))
    .into_response())
}

fn shell_running_mem(form: &[(String, String)]) -> u64 {
    2048
}

// settings page

pub struct NodeOpt2 {
    pub id: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTmpl {
    pub shell: ShellCtx,
    pub message: String,
    pub user_email: String,
    pub is_admin: bool,
    pub nodes: Vec<NodeOpt2>,
    pub mem_mb: u64,
    pub cpu: f64,
    pub disk_mb: u64,
    pub pids_limit: i64,
    pub tags: String,
}

pub async fn settings_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PageQueryMsg>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, _daemon) = shell_guard(&app, &headers, &id, "settings").await?;
    let nodes: Vec<NodeOpt2> = list_nodes(&app)
        .into_iter()
        .filter(|n| n.id != srv.node_id)
        .map(|n| NodeOpt2 { id: n.id, name: n.name })
        .collect();
    Ok(page(&SettingsTmpl {
        message: q.msg.clone().unwrap_or_default(),
        shell,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
        nodes,
        mem_mb: srv.mem_mb,
        cpu: srv.cpu,
        disk_mb: srv.disk_mb,
        pids_limit: srv.pids_limit,
        tags: srv.tags,
    }))
}

pub async fn settings_save(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    admin_guard(&app, &headers)?;
    let new_name = val(&form, "name").trim().to_string();
    if new_name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Name required — <a href='/servers/{id}/settings'>back</a>"),
        )
            .into_response());
    }
    let tags = val(&form, "tags").trim().to_string();
    let (shell, _srv, daemon) = shell_guard(&app, &headers, &id, "settings").await?;

    let mem_mb = val(&form, "mem_mb").parse::<u64>().unwrap_or(2048).max(128);
    let cpu = val(&form, "cpu").parse::<f64>().unwrap_or(2.0).max(0.25);
    let disk_mb = val(&form, "disk_mb").parse::<u64>().unwrap_or(0);
    let pids_limit = val(&form, "pids_limit").parse::<i64>().unwrap_or(0);

    let body = serde_json::json!({
        "name": new_name,
        "limits": {"mem_mb": mem_mb, "cpu_cores": cpu, "disk_mb": disk_mb, "pids_limit": pids_limit},
    });
    daemon
        .update_config(&shell.id, &body)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("save failed: {e:#} — <a href='/servers/{id}/settings'>back</a>"),
            )
                .into_response()
        })?;
    app.db
        .with(|c| {
            c.execute(
                "UPDATE servers SET name=?1, mem_mb=?2, cpu=?3, disk_mb=?4, pids_limit=?5, tags=?6 WHERE id=?7",
                rusqlite::params![new_name, mem_mb as i64, cpu, disk_mb as i64, pids_limit, tags, shell.id],
            )?;
            Ok(())
        })
        .ok();
    Ok(Redirect::to(&format!(
        "/servers/{id}/settings?msg={}",
        urlencoding::encode("Settings saved. Restart to apply resource limits.")
    ))
    .into_response())
}

// ai page

#[derive(Template)]
#[template(path = "ai.html")]
pub struct AiTmpl {
    pub shell: ShellCtx,
    pub ai_error: String,
    pub last_report: Option<ReportView>,
    pub incidents: Vec<ReportView>,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn ai_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PageQueryMsg>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, daemon) = shell_guard(&app, &headers, &id, "ai").await?;
    let raw = daemon.ai_incidents(&srv.id).await.unwrap_or_default();
    let incidents: Vec<crate::routes::proxy::AiIncident> =
        raw.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let reports: Vec<ReportView> = incidents
        .iter()
        .map(|i| ReportView {
            finished_at: i.finished_at.clone(),
            summary: i.summary.clone(),
            actions: i.actions.clone(),
        })
        .collect();
    let ai_error = q.msg.clone().unwrap_or_default();
    Ok(page(&AiTmpl {
        last_report: reports.first().cloned(),
        incidents: reports.into_iter().skip(1).collect(),
        ai_error,
        shell,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}

// ── files browser (shell) ───────────────────────────────────────────────

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

pub struct BackupView {
    pub id: String,
    pub size_mb: String,
    pub created: String,
    pub download_url: String,
}

#[derive(Template)]
#[template(path = "files.html")]
pub struct FilesTmpl {
    pub shell: ShellCtx,
    pub user_email: String,
    pub is_admin: bool,
    pub raw_path: String,
    pub crumbs: Vec<Crumb>,
    pub entries: Vec<FileView>,
    pub sftp_host: String,
    pub sftp_port: u16,
    pub sftp_user: String,
    pub sftp_pass: String,
    pub error: Option<String>,
}

pub struct Crumb {
    pub label: String,
    pub href: String,
}

pub struct FileView {
    pub name: String,
    pub is_dir: bool,
    pub size_text: String,
    pub modified: String,
    pub path_enc: String,
    pub raw_path: String,
    pub is_editable: bool,
}

fn url_host(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => url,
    };
    let host_port = rest.split('/').next().unwrap_or(rest);
    host_port
        .split(':')
        .next()
        .unwrap_or(host_port)
        .to_string()
}

fn parent_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => trimmed[..i].to_string(),
        None => "/".into(),
    }
}

pub async fn files_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (user_email, is_admin) = nav_ctx(&app, &headers);
    if user_email.is_empty() {
        return Err(Redirect::to("/login").into_response());
    }
    let Some(srv) = get_server(&app, &id) else {
        return Err((StatusCode::NOT_FOUND, "no such server").into_response());
    };
    let Some(node) = get_node(&app, &srv.node_id) else {
        return Err((StatusCode::BAD_GATEWAY, "node missing").into_response());
    };
    let d = DaemonClient::new(app.http.clone(), &node);

    let node2 = node.clone();
    let (ctx, _srv2, _daemon2) = build_shell(&app, &headers, &id, "files").await?;
    let _ = node2;

    let path = q.get("path").cloned().unwrap_or_else(|| "/".to_string());
    match d.list_files(&srv.id, Some(&path)).await {
        Ok(entries) => {
            let mut crumbs = vec![Crumb { label: "/".into(), href: "/".into() }];
            if path != "/" {
                let mut acc = String::new();
                for part in path.trim_start_matches('/').split('/') {
                    acc.push('/');
                    acc.push_str(part);
                    crumbs.push(Crumb {
                        label: part.to_string(),
                        href: urlencoding::encode(&acc).to_string(),
                    });
                }
            }
            let views = entries
                .iter()
                .map(|e| FileView {
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                    size_text: if e.is_dir { "—".into() } else { human_size(e.size) },
                    modified: fmt_ts(e.modified_at),
                    path_enc: urlencoding::encode(&e.path).to_string(),
                    raw_path: e.path.clone(),
                    is_editable: !e.is_dir && e.size <= 512 * 1024,
                })
                .collect();
            let (sftp_user, sftp_pass, sftp_port) = match d.sftp_info(&srv.id).await {
                Ok(v) => (
                    v["username"].as_str().unwrap_or("").to_string(),
                    v["password"].as_str().unwrap_or("").to_string(),
                    v["port"].as_u64().unwrap_or(2022) as u16,
                ),
                Err(_) => (String::new(), String::new(), 2022),
            };
            Ok(page(&FilesTmpl {
                shell: ctx,
                user_email,
                is_admin,
                raw_path: path.clone(),
                crumbs,
                entries: views,
                sftp_host: url_host(&node.url),
                sftp_port,
                sftp_user,
                sftp_pass,
                error: None,
            }))
        }
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            format!("listing failed: {e:#} — <a href='/servers/{id}/files'>back</a>"),
        )
            .into_response()),
    }
}

#[derive(Template)]
#[template(path = "file_edit.html")]
pub struct FileEditTmpl {
    pub shell: ShellCtx,
    pub path: String,
    pub raw_path: String,
    pub content: String,
    pub error: Option<String>,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn file_edit_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, daemon) = shell_guard(&app, &headers, &id, "files").await?;
    let Some(path) = q.get("path") else {
        return Err((StatusCode::BAD_REQUEST, "missing ?path").into_response());
    };
    let (user_email, is_admin) = nav_ctx(&app, &headers);
    match daemon.read_file(&srv.id, path).await {
        Ok(bytes) => Ok(page(&FileEditTmpl {
            shell: shell.clone(),
            path: html_escape(path),
            raw_path: path.clone(),
            content: String::from_utf8_lossy(&bytes).to_string(),
            error: None,
            user_email,
            is_admin,
        })),
        Err(e) => Ok(page(&FileEditTmpl {
            shell: shell.clone(),
            path: html_escape(path),
            raw_path: path.clone(),
            content: String::new(),
            error: Some(format!("{e:#}")),
            user_email: user_email.clone(),
            is_admin,
        })),
    }
}

#[derive(serde::Deserialize)]
pub struct EditSaveForm {
    pub path: String,
    pub content: String,
}

pub async fn file_edit_save(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<EditSaveForm>,
) -> Result<Response, Response> {
    let (shell, srv, daemon) = shell_guard(&app, &headers, &id, "files").await?;
    daemon
        .write_file(&srv.id, &form.path, form.content.clone().into_bytes())
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("save failed: {e:#} — <a href='/servers/{id}/files'>back</a>"),
            )
                .into_response()
        })?;
    Ok(Redirect::to(&format!(
        "/servers/{id}/files?path={}",
        urlencoding::encode(&parent_of(&form.path))
    ))
    .into_response())
}

// ── schedules page ───────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "schedules.html")]
pub struct SchedulesTmpl {
    pub shell: ShellCtx,
    pub tasks: Vec<ScheduleView>,
    pub message: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct ScheduleView {
    pub id: String,
    pub name: String,
    pub cron: String,
    pub action: String,
    pub payload: String,
    pub enabled: bool,
    pub next_run: String,
    pub last_result: String,
}

pub async fn schedules_page(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PageQueryMsg>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, daemon) = shell_guard(&app, &headers, &id, "schedules").await?;
    let raw = daemon.schedules(&srv.id).await.unwrap_or_default();
    let tasks = raw
        .iter()
        .map(|v| ScheduleView {
            id: v["id"].as_str().unwrap_or("").to_string(),
            name: v["name"].as_str().unwrap_or("?").to_string(),
            cron: v["cron"].as_str().unwrap_or("").to_string(),
            action: v["action"].as_str().unwrap_or("").to_string(),
            payload: v["payload"].as_str().unwrap_or("").to_string(),
            enabled: v["enabled"].as_bool().unwrap_or(false),
            next_run: v["next_run"]
                .as_str()
                .unwrap_or(if v["enabled"].as_bool().unwrap_or(false) { "…" } else { "paused" })
                .to_string(),
            last_result: v["last_result"].as_str().unwrap_or("never run").to_string(),
        })
        .collect();

    Ok(page(&SchedulesTmpl {
        shell,
        tasks,
        message: q.msg.clone().unwrap_or_default(),
        user_email: nav_ctx(&app, &headers).0,
        is_admin: nav_ctx(&app, &headers).1,
    }))
}


// ---------- server access / members ----------

pub struct MemberRow {
    pub user_id: i64,
    pub email: String,
    pub is_owner: bool,
    pub perms: Vec<String>,
}

#[derive(Template)]
#[template(path = "access.html")]
pub struct AccessTmpl {
    pub shell: ShellCtx,
    pub owner_email: String,
    pub members: Vec<MemberRow>,
    pub flags: Vec<&'static str>,
    pub message: String,
    pub message_class: String,
    pub error: String,
    pub user_email: String,
    pub is_admin: bool,
}

fn access_members(app: &App, srv_id: &str) -> Vec<MemberRow> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT u.id, u.email, COALESCE(us.perms,'') FROM user_servers us
                   JOIN users u ON u.id = us.user_id
                   WHERE us.server_id = ?1 ORDER BY u.email"#,
            )?;
            let rows = stmt
                .query_map(rusqlite::params![srv_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            Ok(rows)
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(user_id, email, perms)| MemberRow {
            user_id,
            email,
            is_owner: false,
            perms: perms.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        })
        .collect()
}

pub async fn server_access(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let (shell, srv, _) = shell_guard(&app, &headers, &id, "access").await?;
    let (user_email, is_admin) = nav_ctx(&app, &headers);
    let owner_email = app
        .db
        .with(|c| {
            let mut stmt = c.prepare("SELECT email FROM users WHERE id = ?1")?;
            let mut rows = stmt.query(rusqlite::params![srv.owner_id])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?.unwrap_or_default())
        })
        .unwrap_or_default();
    Ok(page(&AccessTmpl {
        shell,
        owner_email,
        members: access_members(&app, &id),
        flags: crate::perms::FLAGS.to_vec(),
        message: query_msg(&headers),
        message_class: "success".into(),
        error: String::new(),
        user_email,
        is_admin,
    }))
}

pub fn query_msg(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| urlencoding::decode(r.split("?msg=").nth(1).unwrap_or("")).ok())
        .map(|s| s.replace('+', " "))
        .unwrap_or_default()
}

pub async fn access_add(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<Vec<(String, String)>>,
) -> Response {
    if shell_guard(&app, &headers, &id, "access").await.is_err() {
        return Redirect::to("/login").into_response();
    }
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let email = form
        .iter()
        .find(|(k, _)| k == "email")
        .map(|(_, v)| v.trim().to_lowercase())
        .unwrap_or_default();
    let flags: Vec<String> = form
        .iter()
        .filter(|(k, _)| k != "email")
        .map(|(k, _)| k.clone())
        .filter(|k| crate::perms::FLAGS.contains(&k.as_str()))
        .collect();
    if email.is_empty() || flags.is_empty() {
        return Redirect::to(&format!("/servers/{id}/access")).into_response();
    }
    let res = app.db.with(|c| {
        let uid: Option<i64> = c
            .query_row("SELECT id FROM users WHERE email = ?1", rusqlite::params![email], |r| r.get(0))
            .ok();
        let Some(uid) = uid else { return Err(anyhow::anyhow!("no user with that email")) };
        c.execute(
            "INSERT OR REPLACE INTO user_servers (user_id, server_id, perms) VALUES (?1,?2,?3)",
            rusqlite::params![uid, id, flags.join(",")],
        )?;
        Ok(())
    });
    match res {
        Ok(()) => {
            crate::perms::record(&app.db, &user.email, "access.grant", &id, &format!("{email}: {}", flags.join(",")));
            Redirect::to(&format!("/servers/{id}/access")).into_response()
        }
        Err(_) => Redirect::to(&format!("/servers/{id}/access")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct AccessRemoveForm {
    pub user_id: i64,
}

pub async fn access_remove(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<AccessRemoveForm>,
) -> Response {
    if shell_guard(&app, &headers, &id, "access").await.is_err() {
        return Redirect::to("/login").into_response();
    }
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let _ = app.db.with(|c| {
        c.execute(
            "DELETE FROM user_servers WHERE user_id=?1 AND server_id=?2",
            rusqlite::params![form.user_id, id],
        )?;
        Ok(())
    });
    crate::perms::record(&app.db, &user.email, "access.revoke", &id, &format!("uid {}", form.user_id));
    Redirect::to(&format!("/servers/{id}/access")).into_response()
}

// ---------- account page ----------

pub struct ApiKeyView {
    pub id: i64,
    pub name: String,
    pub created_at: String,
    pub last_used: String,
}

#[derive(Template)]
#[template(path = "account.html")]
pub struct AccountTmpl {
    pub user_email: String,
    pub is_admin: bool,
    pub message: String,
    pub error: String,
    pub api_keys: Vec<ApiKeyView>,
    pub totp_enabled: bool,
    pub totp_setup: bool,
    pub totp_secret: String,
    pub totp_qr: String,
    pub new_api_key: String,
}


fn load_account_err(app: &App, user: &User, msg: &str) -> AccountTmpl {
    let mut t = load_account(app, user.id);
    t.user_email = user.email.clone();
    t.is_admin = user.role == "admin";
    t.totp_enabled = user.totp_enabled;
    t.error = msg.to_string();
    t
}

fn load_account(app: &App, user_id: i64) -> AccountTmpl {
    let keys: Vec<ApiKeyView> = app
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, created_at, last_used FROM api_keys WHERE user_id=?1 ORDER BY id DESC",
            )?;
            let rows = stmt
                .query_map([user_id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            Ok(rows)
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name, created, used)| ApiKeyView {
            id,
            name,
            created_at: fmt_ts(created),
            last_used: used.map(fmt_ts).unwrap_or_else(|| "never".into()),
        })
        .collect();
    AccountTmpl {
        user_email: String::new(),
        is_admin: false,
        message: String::new(),
        error: String::new(),
        api_keys: keys,
        totp_enabled: false,
        totp_setup: false,
        totp_secret: String::new(),
        totp_qr: String::new(),
        new_api_key: String::new(),
    }
}


pub async fn account_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Err(Redirect::to("/login").into_response());
    };
    let mut t = load_account(&app, user.id);
    t.user_email = user.email.clone();
    t.is_admin = user.role == "admin";
    t.totp_enabled = user.totp_enabled;
    t.message = String::new();
    t.error = String::new();
    Ok(page(&t))
}

#[derive(serde::Deserialize)]
pub struct PasswordForm {
    pub current: String,
    pub new: String,
}

pub async fn account_password(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<PasswordForm>,
) -> Response {
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let ok = app
        .db
        .with(|c| {
            let hash: String = c.query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                rusqlite::params![user.id],
                |r| r.get(0),
            )?;
            Ok(hash)
        })
        .map(|h| crate::auth::verify_password(&form.current, &h))
        .unwrap_or(false);
    if !ok {
        return page(&AccountTmpl {
            user_email: user.email.clone(),
            is_admin: user.role == "admin",
            message: String::new(),
            api_keys: Vec::new(),
            totp_enabled: user.totp_enabled,
            totp_setup: false,
            totp_secret: String::new(),
            totp_qr: String::new(),
            new_api_key: String::new(),
            error: "Current password is incorrect.".into(),
        })
        .into_response();
    }
    if form.new.len() < 8 {
        return page(&AccountTmpl {
            user_email: user.email.clone(),
            is_admin: user.role == "admin",
            message: String::new(),
            api_keys: Vec::new(),
            totp_enabled: user.totp_enabled,
            totp_setup: false,
            totp_secret: String::new(),
            totp_qr: String::new(),
            new_api_key: String::new(),
            error: "New password must be at least 8 characters.".into(),
        })
        .into_response();
    }
    let new_hash = match crate::auth::hash_password(&form.new) {
        Ok(h) => h,
        Err(e) => {
            return page(&AccountTmpl {
                user_email: user.email.clone(),
                is_admin: user.role == "admin",
                message: String::new(),
                api_keys: Vec::new(),
                totp_enabled: user.totp_enabled,
                totp_setup: false,
                totp_secret: String::new(),
                totp_qr: String::new(),
                new_api_key: String::new(),
                error: format!("hash failed: {e}"),
            })
            .into_response()
        }
    };
    let _ = app.db.with(|c| {
        c.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            rusqlite::params![new_hash, user.id],
        )?;
        Ok(())
    });
    // invalidate other sessions of this user
    let _ = app.db.with(|c| {
        c.execute(
            "DELETE FROM sessions WHERE user_id = ?1",
            rusqlite::params![user.id],
        )?;
        Ok(())
    });
    crate::perms::record(&app.db, &user.email, "account.password_change", &user.email, "");
    let token = crate::auth::Sessions::create(&app.db, user.id).unwrap_or_default();
    let mut resp = Redirect::to("/account").into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&crate::auth::Sessions::session_cookie(&token))
            .unwrap(),
    );
    resp
}

pub fn query_msg_pub(headers: &HeaderMap) -> (String, bool) {
    headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| {
            let err = r.contains("?err=");
            let part = r.split("?msg=").nth(1).or_else(|| r.split("?err=").nth(1))?;
            urlencoding::decode(part).ok().map(|s| (s.replace('+', " "), err))
        })
        .unwrap_or_default()
}

// ---------- API keys ----------

#[derive(serde::Deserialize)]
pub struct ApiKeyForm {
    pub name: String,
}

pub async fn apikey_create(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<ApiKeyForm>,
) -> Response {
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Redirect::to("/account?err=Name%20required").into_response();
    }
    let (raw, hash) = crate::auth::gen_api_key();
    let res = app.db.with(|c| {
        c.execute(
            "INSERT INTO api_keys (user_id, name, key_hash, created_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![user.id, name, hash, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    });
    let mut t = load_account(&app, user.id);
    t.user_email = user.email.clone();
    t.is_admin = user.role == "admin";
    t.totp_enabled = user.totp_enabled;
    match res {
        Ok(()) => {
            t.new_api_key = raw;
            t.message = "API key created. Copy it now — it won't be shown again.".into();
        }
        Err(_) => t.error = "Could not create key.".into(),
    }
    page(&t).into_response()
}

#[derive(serde::Deserialize)]
pub struct ApiKeyDeleteForm {
    pub key_id: i64,
}

pub async fn apikey_delete(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<ApiKeyDeleteForm>,
) -> Response {
    let Some(user) = crate::auth::Sessions::user_for(&app.db, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let _ = app.db.with(|c| {
        c.execute(
            "DELETE FROM api_keys WHERE id=?1 AND user_id=?2",
            rusqlite::params![form.key_id, user.id],
        )?;
        Ok(())
    });
    Redirect::to("/account").into_response()
}

// ---------- 2FA ----------

#[derive(Template)]
#[template(path = "login_2fa.html")]
pub struct TwoFactorTmpl {
    pub error: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn login_2fa_get() -> Response {
    page(&TwoFactorTmpl { error: String::new(), user_email: String::new(), is_admin: false })
}

pub async fn login_2fa_post(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let code = form.iter().find(|(k, _)| k == "code").map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let token = auth::cookie_from(&headers, auth::Sessions::PENDING_2FA_COOKIE);
    let Some(token) = token else {
        return Redirect::to("/login").into_response();
    };
    let Some(uid) = auth::consume_pending_2fa(&app.db, &token) else {
        return Redirect::to("/login").into_response();
    };
    let secret: Option<String> = app
        .db
        .with(|c| Ok(c.query_row("SELECT totp_secret FROM users WHERE id=?1", rusqlite::params![uid], |r| r.get(0)).ok()))
        .ok()
        .flatten();
    let Some(secret) = secret else {
        return Redirect::to("/login").into_response();
    };
    if !auth::totp_verify(&secret, &code) {
        // re-issue a pending token so the user can retry
        let ptoken = auth::create_pending_2fa(&app.db, uid);
        return redirect_with_cookie(
            "/login/2fa",
            auth::Sessions::pending_cookie(&ptoken),
        );
    }
    match Sessions::create(&app.db, uid) {
        Ok(session) => redirect_with_cookie("/", Sessions::session_cookie(&session)),
        Err(_) => Redirect::to("/login").into_response(),
    }
}

#[derive(Template)]
#[template(path = "account_2fa.html")]
pub struct Account2faTmpl {
    pub user_email: String,
    pub is_admin: bool,
    pub secret: String,
    pub qr: String,
    pub error: String,
}

pub async fn totp_setup_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let Some(user) = Sessions::user_for(&app.db, &headers) else {
        return Err(Redirect::to("/login").into_response());
    };
    let secret: Option<String> = app
        .db
        .with(|c| Ok(c.query_row("SELECT totp_secret FROM users WHERE id=?1", rusqlite::params![user.id], |r| r.get(0)).ok()))
        .ok()
        .flatten();
    let Some(secret) = secret else {
        return Err(Redirect::to("/account").into_response());
    };
    let issuer = app.cfg.app_name.clone();
    let uri = auth::totp_uri(&secret, &user.email, &issuer);
    let qr = QrCode::new(uri)
        .ok()
        .map(|q| q.render::<svg::Color>().module_dimensions(8, 8).build())
        .unwrap_or_default();
    Ok(page(&Account2faTmpl {
        user_email: user.email.clone(),
        is_admin: user.role == "admin",
        secret,
        qr,
        error: String::new(),
    }))
}

pub async fn totp_enable(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let Some(user) = Sessions::user_for(&app.db, &headers) else {
        return Err(Redirect::to("/login").into_response());
    };
    let secret = auth::totp_new_secret();
    let _ = app.db.with(|c| {
        c.execute(
            "UPDATE users SET totp_secret=?1, totp_enabled=0 WHERE id=?2",
            rusqlite::params![secret, user.id],
        )?;
        Ok(())
    });
    Ok(Redirect::to("/account/2fa/setup").into_response())
}

pub async fn totp_confirm(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    let Some(user) = Sessions::user_for(&app.db, &headers) else {
        return Err(Redirect::to("/login").into_response());
    };
    let code = form.iter().find(|(k, _)| k == "code").map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let secret: Option<String> = app
        .db
        .with(|c| Ok(c.query_row("SELECT totp_secret FROM users WHERE id=?1", rusqlite::params![user.id], |r| r.get(0)).ok()))
        .ok()
        .flatten();
    match secret {
        Some(secret) if auth::totp_verify(&secret, &code) => {
            let _ = app.db.with(|c| {
                c.execute("UPDATE users SET totp_enabled=1 WHERE id=?1", rusqlite::params![user.id])?;
                Ok(())
            });
            crate::perms::record(&app.db, &user.email, "account.2fa_enable", &user.email, "");
            Ok(Redirect::to("/account?msg=Two-factor%20enabled").into_response())
        }
        _ => Ok(Redirect::to("/account/2fa/setup?err=Invalid%20code").into_response()),
    }
}

pub async fn totp_disable(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    let Some(user) = Sessions::user_for(&app.db, &headers) else {
        return Err(Redirect::to("/login").into_response());
    };
    let current = form.iter().find(|(k, _)| k == "current").map(|(_, v)| v.clone()).unwrap_or_default();
    let ok = app
        .db
        .with(|c| {
            let hash: String = c.query_row("SELECT password_hash FROM users WHERE id=?1", rusqlite::params![user.id], |r| r.get(0))?;
            Ok(hash)
        })
        .map(|h| auth::verify_password(&current, &h))
        .unwrap_or(false);
    if !ok {
        return Ok(page(&load_account_err(&app, &user, "Current password is incorrect.")).into_response());
    }
    let _ = app.db.with(|c| {
        c.execute("UPDATE users SET totp_enabled=0, totp_secret='' WHERE id=?1", rusqlite::params![user.id])?;
        Ok(())
    });
    crate::perms::record(&app.db, &user.email, "account.2fa_disable", &user.email, "");
    Ok(Redirect::to("/account?msg=Two-factor%20disabled").into_response())
}
