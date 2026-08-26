use super::pages::{nav_ctx, page};
use super::*;
use crate::daemon::DaemonClient;
use askama::Template;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};

#[derive(Template)]
#[template(path = "admin_nodes.html")]
pub struct NodesTmpl {
    pub message: String,
    pub message_class: String,
    pub nodes: Vec<NodeView>,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct NodeView {
    pub id: String,
    pub name: String,
    pub url: String,
    #[allow(dead_code)]
    pub token: String,
    pub alias: String,
    pub status_class: String,
    pub status_text: String,
}

#[derive(serde::Deserialize)]
pub struct AdminQuery {
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub err: Option<String>,
}

pub async fn nodes_page(
    State(app): State<SharedApp>,
    Query(q): Query<AdminQuery>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let mut nodes = Vec::new();
    for n in list_nodes(&app) {
        let client = DaemonClient::new(app.http.clone(), &n);
        let online = tokio::time::timeout(std::time::Duration::from_secs(4), client.health())
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
        let (status_class, status_text) = if online {
            ("green".into(), "Online".into())
        } else {
            ("red".into(), "Unreachable".into())
        };
        nodes.push(NodeView {
            id: n.id.clone(),
            name: n.name.clone(),
            url: n.url.clone(),
            token: n.token.clone(),
            alias: n.alias.clone(),
            status_class,
            status_text,
        });
    }

    let (message, message_class) = if let Some(e) = q.err {
        (e, "error".to_string())
    } else {
        (q.msg.unwrap_or_default(), "muted".to_string())
    };

    page(&NodesTmpl {
        message,
        message_class,
        nodes,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: true,
    })
}

#[derive(serde::Deserialize)]
pub struct NodeForm {
    pub name: String,
    pub url: String,
    pub token: String,
    #[serde(default)]
    pub alias: Option<String>,
}

pub async fn nodes_add(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<NodeForm>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let url = form.url.trim_end_matches('/').to_string();
    let id = nucleus_core::new_server_id();
    let res = app.db.with(|c| {
        c.execute(
            "INSERT INTO nodes (id, name, url, token, created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                id,
                form.name.trim(),
                url,
                form.token.trim(),
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(())
    });
    match res {
        Ok(()) => Redirect::to(&format!(
            "/admin/nodes?msg={}",
            urlencoding::encode(&format!("Node '{}' registered.", form.name))
        ))
        .into_response(),
        Err(e) => Redirect::to(&format!(
            "/admin/nodes?err={}",
            urlencoding::encode(&format!("Insert failed: {e}"))
        ))
        .into_response(),
    }
}

// ---------- eggs ----------

#[derive(Template)]
#[template(path = "admin_eggs.html")]
pub struct EggsTmpl {
    pub message: String,
    pub message_class: String,
    pub eggs: Vec<EggView>,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct EggView {
    pub name: String,
    pub slug: String,
    pub images: String,
    pub vars: usize,
}

pub async fn eggs_page(
    State(app): State<SharedApp>,
    Query(q): Query<AdminQuery>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let eggs = list_eggs(&app)
        .iter()
        .map(|e| EggView {
            name: e.name.clone(),
            slug: e.slug.clone(),
            images: e.egg.docker_images.join(", "),
            vars: e.egg.variables.len(),
        })
        .collect();

    let (message, message_class) = if let Some(e) = q.err {
        (e, "error".to_string())
    } else {
        (q.msg.unwrap_or_default(), "muted".to_string())
    };

    page(&EggsTmpl {
        message,
        message_class,
        eggs,
        user_email: nav_ctx(&app, &headers).0,
        is_admin: true,
    })
}

pub async fn eggs_import(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let mut ok = 0usize;
    let mut failed = 0usize;
    while let Some(field) = mp.next_field().await.ok().flatten() {
        if field.name() != Some("egg") {
            continue;
        }
        match field.text().await {
            Ok(raw) => match nucleus_core::import_ptero_egg(&raw) {
                Ok(egg) => {
                    let res = app.db.with(|c| {
                        c.execute(
                            "INSERT INTO eggs (slug, name, json, created_at) VALUES (?1,?2,?3,?4)
                             ON CONFLICT(slug) DO UPDATE SET name=?2, json=?3",
                            rusqlite::params![
                                egg.slug,
                                egg.name,
                                serde_json::to_string(&egg).unwrap_or_default(),
                                chrono::Utc::now().timestamp()
                            ],
                        )?;
                        Ok(())
                    });
                    if res.is_ok() {
                        ok += 1;
                        tracing::info!(slug = %egg.slug, "egg imported");
                    } else {
                        failed += 1;
                    }
                }
                Err(_) => failed += 1,
            },
            Err(_) => failed += 1,
        }
    }

    let msg = format!("Imported {ok} egg(s); {failed} failed.");
    Redirect::to(&format!("/admin/eggs?msg={}", urlencoding::encode(&msg))).into_response()
}

#[derive(serde::Deserialize)]
pub struct AliasForm {
    pub alias: String,
}

pub async fn nodes_set_alias(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<AliasForm>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let res = app.db.with(|c| {
        c.execute(
            "UPDATE nodes SET alias=?1 WHERE id=?2",
            rusqlite::params![form.alias.trim(), id],
        )?;
        Ok(())
    });
    match res {
        Ok(()) => Redirect::to(&format!(
            "/admin/nodes?msg={}",
            urlencoding::encode("Alias updated.")
        ))
        .into_response(),
        Err(e) => Redirect::to(&format!(
            "/admin/nodes?err={}",
            urlencoding::encode(&format!("Update failed: {e}"))
        ))
        .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct NodeEditForm {
    pub name: String,
    pub url: String,
    pub token: String,
    pub alias: String,
}

pub async fn nodes_edit(
    State(app): State<SharedApp>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<NodeEditForm>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let url = form.url.trim_end_matches('/').to_string();
    let res = app.db.with(|c| {
        c.execute(
            "UPDATE nodes SET name=?1, url=?2, token=?3, alias=?4 WHERE id=?5",
            rusqlite::params![form.name.trim(), url, form.token.trim(), form.alias.trim(), id],
        )?;
        Ok(())
    });
    match res {
        Ok(()) => Redirect::to("/admin/nodes?msg=Node%20updated.").into_response(),
        Err(e) => Redirect::to(&format!(
            "/admin/nodes?err={}",
            urlencoding::encode(&format!("Update failed: {e}"))
        ))
        .into_response(),
    }
}


// ---------- user management ----------

pub struct UserRow {
    pub id: i64,
    pub email: String,
    pub is_admin: bool,
    pub created: String,
    pub server_count: i64,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
pub struct UsersTmpl {
    pub users: Vec<UserRow>,
    pub message: String,
    pub message_class: String,
    pub error: String,
    pub me_email: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn users_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let (me_email, _) = nav_ctx(&app, &headers);
    let rows: Vec<UserRow> = app
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT u.id, u.email, u.role, u.created_at,
                          (SELECT COUNT(*) FROM servers s WHERE s.owner_id = u.id)
                   FROM users u ORDER BY u.created_at"#,
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .map(|(id, email, role, ts, cnt)| UserRow {
                    id,
                    email,
                    is_admin: role == "admin",
                    created: fmt_date(ts),
                    server_count: cnt,
                })
                .collect();
            Ok(rows)
        })
        .unwrap_or_default();
    page(&UsersTmpl {
        users: rows,
        message: crate::routes::pages::query_msg(&headers),
        message_class: "success".into(),
        error: String::new(),
        me_email,
        user_email: String::new(),
        is_admin: true,
    })
}

fn fmt_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn msg_from_headers(headers: &HeaderMap) -> (String, String) {
    let m = crate::routes::pages::query_msg_pub(headers);
    (m.0, if m.1 { "error".into() } else { "success".into() })
}

#[derive(serde::Deserialize)]
pub struct UserCreateForm {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub admin: Option<String>,
}

pub async fn users_create(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<UserCreateForm>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let actor = crate::auth::Sessions::user_for(&app.db, &headers);
    let email = form.email.trim().to_lowercase();
    if !email.contains('@') || form.password.len() < 8 {
        return Redirect::to("/admin/users?err=Valid%20email%20and%208%2B-char%20password%20required.").into_response();
    }
    let hash = match crate::auth::hash_password(&form.password) {
        Ok(h) => h,
        Err(_) => return Redirect::to("/admin/users?err=Hash%20failed.").into_response(),
    };
    let role = if form.admin.as_deref() == Some("1") { "admin" } else { "user" };
    let res = app.db.with(|c| {
        c.execute(
            "INSERT INTO users (email, password_hash, role, created_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![email, hash, role, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    });
    match res {
        Ok(_) => {
            if let Some(a) = actor {
                crate::perms::record(&app.db, &a.email, "user.create", &email, &format!("role={role}"));
            }
            Redirect::to("/admin/users?msg=User%20created.").into_response()
        }
        Err(e) => Redirect::to(&format!(
            "/admin/users?err={}",
            urlencoding::encode(&format!("Create failed: {e} — email already taken?"))
        ))
        .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct UserResetForm {
    pub password: String,
}

pub async fn users_reset(
    State(app): State<SharedApp>,
    AxumPath(uid): AxumPath<i64>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<UserResetForm>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    if form.password.len() < 8 {
        return Redirect::to("/admin/users?err=Password%20must%20be%208%2B%20chars.").into_response();
    }
    let hash = match crate::auth::hash_password(&form.password) {
        Ok(h) => h,
        Err(_) => return Redirect::to("/admin/users?err=Hash%20failed.").into_response(),
    };
    let _ = app.db.with(|c| {
        c.execute(
            "UPDATE users SET password_hash=?1 WHERE id=?2",
            rusqlite::params![hash, uid],
        )?;
        // kick their sessions
        c.execute("DELETE FROM sessions WHERE user_id=?1", rusqlite::params![uid])?;
        Ok(())
    });
    if let Some(a) = crate::auth::Sessions::user_for(&app.db, &headers) {
        crate::perms::record(&app.db, &a.email, "user.password_reset", &format!("uid {uid}"), "");
    }
    Redirect::to("/admin/users?msg=Password%20reset%20and%20sessions%20cleared.").into_response()
}

pub async fn users_role_toggle(
    State(app): State<SharedApp>,
    AxumPath(uid): AxumPath<i64>,
    headers: HeaderMap,
) -> Response {
    let me = crate::auth::Sessions::user_for(&app.db, &headers);
    if me.as_ref().map(|u| u.role != "admin").unwrap_or(true) || me.as_ref().map(|u| u.id == uid).unwrap_or(false) {
        return Redirect::to("/admin/users?err=Cannot%20change%20this%20account.").into_response();
    }
    let _ = app.db.with(|c| {
        c.execute(
            "UPDATE users SET role = CASE role WHEN 'admin' THEN 'user' ELSE 'admin' END WHERE id=?1",
            rusqlite::params![uid],
        )?;
        Ok(())
    });
    if let Some(a) = me {
        crate::perms::record(&app.db, &a.email, "user.role_toggle", &format!("uid {uid}"), "");
    }
    Redirect::to("/admin/users?msg=Role%20updated.").into_response()
}

pub async fn users_delete(
    State(app): State<SharedApp>,
    AxumPath(uid): AxumPath<i64>,
    headers: HeaderMap,
) -> Response {
    let me = crate::auth::Sessions::user_for(&app.db, &headers);
    if me.as_ref().map(|u| u.role != "admin").unwrap_or(true) || me.as_ref().map(|u| u.id == uid).unwrap_or(false) {
        return Redirect::to("/admin/users?err=Cannot%20delete%20yourself.").into_response();
    }
    let _ = app.db.with(|c| {
        c.execute("DELETE FROM sessions WHERE user_id=?1", rusqlite::params![uid])?;
        c.execute("DELETE FROM user_servers WHERE user_id=?1", rusqlite::params![uid])?;
        // orphaned owned servers keep owner_id; set NULL so they remain visible to admins
        c.execute("UPDATE servers SET owner_id=NULL WHERE owner_id=?1", rusqlite::params![uid])?;
        c.execute("DELETE FROM users WHERE id=?1", rusqlite::params![uid])?;
        Ok(())
    });
    if let Some(a) = me {
        crate::perms::record(&app.db, &a.email, "user.delete", &format!("uid {uid}"), "");
    }
    Redirect::to("/admin/users?msg=User%20deleted.").into_response()
}

// ---------- activity log ----------

pub struct ActivityRow {
    pub when: String,
    pub email: String,
    pub action: String,
    pub target: String,
    pub detail: String,
}

#[derive(Template)]
#[template(path = "activity.html")]
pub struct ActivityTmpl {
    pub rows: Vec<ActivityRow>,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn activity_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let (user_email, _) = nav_ctx(&app, &headers);
    let rows: Vec<ActivityRow> = app
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT ts, email, action, target, detail FROM activity ORDER BY id DESC LIMIT 300",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
                })?
                .filter_map(|r| r.ok())
                .map(|(ts, email, action, target, detail)| ActivityRow {
                    when: chrono::DateTime::from_timestamp(ts, 0)
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                        .unwrap_or_default(),
                    email,
                    action,
                    target,
                    detail,
                })
                .collect();
            Ok(rows)
        })
        .unwrap_or_default();
    page(&ActivityTmpl { rows, user_email, is_admin: true })
}

// ---------- invites ----------

#[derive(Template)]
#[template(path = "admin_invites.html")]
pub struct InvitesTmpl {
    pub message: String,
    pub error: String,
    pub invited_link: String,
    pub invites: Vec<InviteView>,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct InviteView {
    pub email: String,
    pub role: String,
    pub created: String,
    pub token: String,
}

#[derive(serde::Deserialize)]
pub struct InviteForm {
    pub email: String,
    #[serde(default)]
    pub role: Option<String>,
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn load_invites(app: &App) -> Vec<InviteView> {
    app.db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT email, role, created_at, token FROM invites WHERE used_at IS NULL ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
                })?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            Ok(rows)
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(email, role, created, token)| InviteView {
            email,
            role,
            created: fmt_ts(created),
            token,
        })
        .collect()
}

pub async fn invites_page(
    State(app): State<SharedApp>,
    Query(q): Query<AdminQuery>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let (user_email, is_admin) = nav_ctx(&app, &headers);
    let mut t = InvitesTmpl {
        message: q.msg.clone().unwrap_or_default(),
        error: q.err.clone().unwrap_or_default(),
        invited_link: String::new(),
        invites: load_invites(&app),
        user_email,
        is_admin,
    };
    if let Some(link) = q.msg.as_ref().and_then(|m| m.strip_prefix("link:")) {
        t.invited_link = link.to_string();
    }
    page(&t)
}

pub async fn invites_create(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<InviteForm>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let email = form.email.trim().to_lowercase();
    if !email.contains('@') {
        return Redirect::to("/admin/invites?err=Valid%20email%20required").into_response();
    }
    let role = if form.role.as_deref() == Some("admin") { "admin" } else { "user" };
    let token = crate::auth::new_token();
    let actor_email = crate::auth::Sessions::user_for(&app.db, &headers).map(|u| u.email).unwrap_or_default();
    let res = app.db.with(|c| {
        c.execute(
            "INSERT INTO invites (token, email, role, invited_by, created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![token, email, role, actor_email.clone(), chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    });
    if res.is_err() {
        return Redirect::to("/admin/invites?err=Could%20not%20create%20invite").into_response();
    }
    let link = format!("http://localhost:8025/register?invite={}", token);
    // attempt to mail it if SMTP is configured
    if let Some(smtp) = &app.cfg.smtp {
        if let Ok(body) = build_invite_email(&email, &link, &app.cfg.app_name) {
            if let Err(e) = send_invite_email(smtp, &email, &body).await {
                tracing::warn!(error=%e, "failed to send invite email");
            }
        }
    }
    crate::perms::record(&app.db, &actor_email, "invite.create", &email, &role);
    Redirect::to(&format!("/admin/invites?msg={}", urlencoding::encode(&format!("link:{link}")))).into_response()
}

pub async fn invites_revoke(
    State(app): State<SharedApp>,
    AxumPath(token): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let _ = app.db.with(|c| {
        c.execute("DELETE FROM invites WHERE token=?1", rusqlite::params![token])?;
        Ok(())
    });
    Redirect::to("/admin/invites").into_response()
}

fn build_invite_email(to: &str, link: &str, app_name: &str) -> anyhow::Result<String> {
    Ok(format!(
        "You've been invited to join {app_name}.\n\nCreate your account here:\n{link}\n\nThis invite can only be used once."
    ))
}

async fn send_invite_email(
    smtp: &crate::config::SmtpConfig,
    to: &str,
    body: &str,
) -> anyhow::Result<()> {
    use lettre::message::header::ContentType;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
    let from = smtp
        .from
        .parse::<lettre::message::Mailbox>()
        .map_err(|e| anyhow::anyhow!("from address: {e}"))?;
    let to_mb = to
        .parse::<lettre::message::Mailbox>()
        .map_err(|e| anyhow::anyhow!("to address: {e}"))?;
    let email = Message::builder()
        .from(from)
        .to(to_mb)
        .subject("You've been invited to Nucleus")
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())?;
    let mailer = if smtp.tls {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)?
    }
    .port(smtp.port);
    let mailer = if let (Some(u), Some(p)) = (smtp.user.as_ref(), smtp.password.as_ref()) {
        use lettre::transport::smtp::authentication::Credentials;
        mailer.credentials(Credentials::new(u.clone(), p.clone()))
    } else {
        mailer
    }
    .build();
    mailer.send(email).await?;
    Ok(())
}
