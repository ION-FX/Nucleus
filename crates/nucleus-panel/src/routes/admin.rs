use super::pages::{nav_ctx, page};
use super::*;
use crate::daemon::DaemonClient;
use askama::Template;
use axum::extract::{Form, Multipart, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
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
        me_email: me_email.clone(),
        user_email: me_email,
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

pub async fn activity_export(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let format = q.get("format").cloned().unwrap_or_else(|| "csv".into());
    let rows: Vec<(i64, String, String, String, String)> = app
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT ts, email, action, target, detail FROM activity ORDER BY id DESC LIMIT 10000",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .unwrap_or_default();

    if format == "json" {
        let json: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(ts, email, action, target, detail)| {
                serde_json::json!({
                    "timestamp": chrono::DateTime::from_timestamp(ts, 0)
                        .map(|d| d.to_rfc3339()).unwrap_or_default(),
                    "email": email,
                    "action": action,
                    "target": target,
                    "detail": detail,
                })
            })
            .collect();
        let body = serde_json::to_string_pretty(&json).unwrap_or_else(|_| "[]".into());
        (
            [(axum::http::header::CONTENT_TYPE, "application/json".to_string())],
            body,
        )
            .into_response()
    } else {
        let mut csv = String::from("timestamp,email,action,target,detail\n");
        for (ts, email, action, target, detail) in &rows {
            let when = chrono::DateTime::from_timestamp(*ts, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_default();
            csv.push_str(&format!(
                "{},{},{},{},{}\n",
                csv_escape(&when),
                csv_escape(email),
                csv_escape(action),
                csv_escape(target),
                csv_escape(detail),
            ));
        }
        (
            [(axum::http::header::CONTENT_TYPE, "text/csv".to_string())],
            csv,
        )
            .into_response()
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
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

// ---------- admin defaults ----------

#[derive(Template)]
#[template(path = "admin_defaults.html")]
pub struct DefaultsTmpl {
    pub mem_mb: u64,
    pub cpu: f64,
    pub disk_mb: u64,
    pub pids_limit: i64,
    pub message: String,
    pub user_email: String,
    pub is_admin: bool,
}

fn get_setting(app: &App, key: &str) -> Option<String> {
    app.db
        .with(|c| Ok(c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).ok()))
        .ok()
        .flatten()
}

fn set_setting(app: &App, key: &str, value: &str) {
    let _ = app.db.with(|c| {
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            rusqlite::params![key, value],
        )?;
        Ok(())
    });
}

pub async fn defaults_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let (user_email, _) = nav_ctx(&app, &headers);
    let mem_mb = get_setting(&app, "default_mem_mb").and_then(|v| v.parse().ok()).unwrap_or(2048);
    let cpu = get_setting(&app, "default_cpu").and_then(|v| v.parse().ok()).unwrap_or(2.0);
    let disk_mb = get_setting(&app, "default_disk_mb").and_then(|v| v.parse().ok()).unwrap_or(0);
    let pids_limit = get_setting(&app, "default_pids_limit").and_then(|v| v.parse().ok()).unwrap_or(0);
    page(&DefaultsTmpl {
        mem_mb,
        cpu,
        disk_mb,
        pids_limit,
        message: String::new(),
        user_email,
        is_admin: true,
    })
}

pub async fn defaults_save(
    State(app): State<SharedApp>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let mem_mb = crate::routes::pages::val(&form, "mem_mb").parse::<u64>().unwrap_or(2048).max(128);
    let cpu = crate::routes::pages::val(&form, "cpu").parse::<f64>().unwrap_or(2.0).max(0.25);
    let disk_mb = crate::routes::pages::val(&form, "disk_mb").parse::<u64>().unwrap_or(0);
    let pids_limit = crate::routes::pages::val(&form, "pids_limit").parse::<i64>().unwrap_or(0);
    set_setting(&app, "default_mem_mb", &mem_mb.to_string());
    set_setting(&app, "default_cpu", &cpu.to_string());
    set_setting(&app, "default_disk_mb", &disk_mb.to_string());
    set_setting(&app, "default_pids_limit", &pids_limit.to_string());
    let (user_email, _) = nav_ctx(&app, &headers);
    page(&DefaultsTmpl {
        mem_mb,
        cpu,
        disk_mb,
        pids_limit,
        message: "Defaults saved.".into(),
        user_email,
        is_admin: true,
    })
}

// ---------- admin dashboard ----------

/// Full panel version (semver + git SHA) for daemon skew detection.
pub fn panel_version() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("NUCLEUS_GIT_SHA").unwrap_or("dev")
    )
}

pub struct NodeDash {
    pub id: String,
    pub name: String,
    pub alias: String,
    pub online: bool,
    pub hostname: String,
    pub daemon_version: String,
    pub daemon_outdated: bool,
    pub docker_version: String,
    pub cpu_cores: u64,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub mem_percent: f64,
    pub mem_pct: i64,
    pub disk_total_gb: u64,
    pub disk_used_gb: u64,
    pub disk_percent: f64,
    pub disk_pct: i64,
    pub servers: usize,
    pub running: usize,
    pub alloc_mem_mb: u64,
    pub alloc_cpu: f64,
    pub alloc_disk_mb: u64,
    pub uptime_days: String,
}

#[derive(Template)]
#[template(path = "admin_dashboard.html")]
pub struct DashboardTmpl {
    pub fleet: FleetStats,
    pub nodes: Vec<NodeDash>,
    pub recent: Vec<ActivityRow>,
    pub user_email: String,
    pub is_admin: bool,
}

pub struct FleetStats {
    pub nodes_total: usize,
    pub nodes_online: usize,
    pub servers: usize,
    pub running: usize,
    pub users: usize,
    pub alloc_mem_mb: u64,
    pub alloc_cpu: f64,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub mem_pct: i64,
}

async fn collect_node_stats(app: &App) -> (Vec<NodeDash>, FleetStats) {
    let nodes = list_nodes(app);
    // per-node allocation sums from the DB
    let mut alloc: std::collections::HashMap<String, (usize, u64, f64, u64)> =
        std::collections::HashMap::new();
    let _ = app.db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT node_id, COUNT(*), COALESCE(SUM(mem_mb),0), COALESCE(SUM(cpu),0), COALESCE(SUM(disk_mb),0) FROM servers GROUP BY node_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)? as u64,
            ))
        })?;
        for r in rows.filter_map(|r| r.ok()) {
            alloc.insert(r.0.clone(), (r.1, r.2, r.3, r.4));
        }
        Ok(())
    });

    let users: usize = app
        .db
        .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))? as usize))
        .unwrap_or(0);

    let mut dashes: Vec<NodeDash> = Vec::new();
    let clients: Vec<DaemonClient> = nodes
        .iter()
        .map(|n| DaemonClient::new(app.http.clone(), n))
        .collect();
    let mut futs = Vec::new();
    for d in &clients {
        futs.push(tokio::time::timeout(std::time::Duration::from_secs(6), d.info()));
    }
    let results = futures_util::future::join_all(futs).await;

    let servers_total: usize = alloc.values().map(|a| a.0).sum();
    let (mut fleet_alloc_mem, mut fleet_alloc_cpu) = (0u64, 0f64);
    let (mut online, mut running_total) = (0usize, 0usize);
    let (mut fleet_mem_total, mut fleet_mem_used) = (0u64, 0u64);

    for (n, res) in nodes.iter().zip(results.into_iter()) {
        let (count, amem, acpu, adisk) = alloc.get(&n.id).cloned().unwrap_or((0, 0, 0.0, 0));
        fleet_alloc_mem += amem;
        fleet_alloc_cpu += acpu;
        let info = match res {
            Ok(Ok(v)) => Some(v),
            _ => None,
        };
        let (online_now, dash_running) = if let Some(v) = &info {
            let r = v.get("running").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            running_total += r;
            (true, r)
        } else {
            (false, 0)
        };
        if online_now {
            online += 1;
        }
        let mem_total = info.as_ref().and_then(|v| v.get("mem_total_mb")).and_then(|x| x.as_u64()).unwrap_or(0);
        let mem_used = info.as_ref().and_then(|v| v.get("mem_used_mb")).and_then(|x| x.as_u64()).unwrap_or(0);
        if online_now {
            fleet_mem_total += mem_total;
            fleet_mem_used += mem_used;
        }
        let disk_total = info.as_ref().and_then(|v| v.get("disk_total_gb")).and_then(|x| x.as_u64()).unwrap_or(0);
        let disk_used = info.as_ref().and_then(|v| v.get("disk_used_gb")).and_then(|x| x.as_u64()).unwrap_or(0);
        let uptime = info.as_ref().and_then(|v| v.get("uptime_secs")).and_then(|x| x.as_u64()).unwrap_or(0);
        let daemon_version =
            info.as_ref().and_then(|v| v.get("daemon_version")).and_then(|x| x.as_str()).unwrap_or("—").to_string();
        // Flag version skew between panel and daemon — mismatched builds cause
        // subtle breakage (e.g. a daemon missing installer image pulls).
        // Version strings embed the git SHA so different builds never compare
        // equal.
        let daemon_outdated = daemon_version != "—" && daemon_version != panel_version();
        dashes.push(NodeDash {
            id: n.id.clone(),
            name: n.name.clone(),
            alias: n.alias.clone(),
            online: online_now,
            hostname: info.as_ref().and_then(|v| v.get("hostname")).and_then(|x| x.as_str()).unwrap_or("—").to_string(),
            daemon_version,
            daemon_outdated,
            docker_version: info.as_ref().and_then(|v| v.get("docker_version")).and_then(|x| x.as_str()).unwrap_or("—").to_string(),
            cpu_cores: info.as_ref().and_then(|v| v.get("cpu_cores")).and_then(|x| x.as_u64()).unwrap_or(0),
            mem_total_mb: mem_total,
            mem_used_mb: mem_used,
            mem_percent: if mem_total > 0 { mem_used as f64 / mem_total as f64 * 100.0 } else { 0.0 },
            mem_pct: if mem_total > 0 { (mem_used as f64 / mem_total as f64 * 100.0) as i64 } else { 0 },
            disk_total_gb: disk_total,
            disk_used_gb: disk_used,
            disk_percent: if disk_total > 0 { disk_used as f64 / disk_total as f64 * 100.0 } else { 0.0 },
            disk_pct: if disk_total > 0 { (disk_used as f64 / disk_total as f64 * 100.0) as i64 } else { 0 },
            servers: count,
            running: dash_running,
            alloc_mem_mb: amem,
            alloc_cpu: acpu,
            alloc_disk_mb: adisk,
            uptime_days: if uptime > 86400 {
                format!("{:.1} d", uptime as f64 / 86400.0)
            } else if uptime > 3600 {
                format!("{:.1} h", uptime as f64 / 3600.0)
            } else {
                format!("{} min", uptime / 60)
            },
        });
    }

    (
        dashes,
        FleetStats {
            nodes_total: nodes.len(),
            nodes_online: online,
            servers: servers_total,
            running: running_total,
            users,
            alloc_mem_mb: fleet_alloc_mem,
            alloc_cpu: fleet_alloc_cpu,
            mem_total_mb: fleet_mem_total,
            mem_used_mb: fleet_mem_used,
            mem_pct: if fleet_mem_total > 0 { (fleet_mem_used as f64 / fleet_mem_total as f64 * 100.0) as i64 } else { 0 },
        },
    )
}

pub async fn dashboard_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let (user_email, _) = nav_ctx(&app, &headers);
    let (nodes, fleet) = collect_node_stats(&app).await;
    let recent: Vec<ActivityRow> = app
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT ts, email, action, target, detail FROM activity ORDER BY id DESC LIMIT 8",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
                })?
                .filter_map(|r| r.ok())
                .map(|(ts, email, action, target, detail)| ActivityRow {
                    when: chrono::DateTime::from_timestamp(ts, 0)
                        .map(|d| d.format("%m-%d %H:%M").to_string())
                        .unwrap_or_default(),
                    email, action, target, detail,
                })
                .collect();
            Ok(rows)
        })
        .unwrap_or_default();
    page(&DashboardTmpl { fleet, nodes, recent, user_email, is_admin: true })
}

/// JSON variant polled by the dashboard for live updates.
pub async fn dashboard_stats(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let (nodes, fleet) = collect_node_stats(&app).await;
    let nodes_json: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "online": n.online,
                "mem_total_mb": n.mem_total_mb,
                "mem_used_mb": n.mem_used_mb,
                "mem_percent": (n.mem_percent * 10.0).round() / 10.0,
                "disk_total_gb": n.disk_total_gb,
                "disk_used_gb": n.disk_used_gb,
                "disk_percent": (n.disk_percent * 10.0).round() / 10.0,
                "servers": n.servers,
                "running": n.running,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "fleet": {
            "nodes_total": fleet.nodes_total,
            "nodes_online": fleet.nodes_online,
            "servers": fleet.servers,
            "running": fleet.running,
            "users": fleet.users,
            "alloc_mem_mb": fleet.alloc_mem_mb,
            "alloc_cpu": fleet.alloc_cpu,
            "mem_total_mb": fleet.mem_total_mb,
            "mem_used_mb": fleet.mem_used_mb,
        },
        "nodes": nodes_json,
    }))
    .into_response()
}

// ---------- updater ----------

const REPO: &str = "ION-FX/Nucleus";

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn current_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(serde::Deserialize)]
struct GhCommit {
    sha: String,
    #[serde(rename = "commit")]
    commit: GhCommitInfo,
}

#[derive(serde::Deserialize)]
struct GhCommitInfo {
    committer: GhCommitter,
    message: String,
}

#[derive(serde::Deserialize)]
struct GhCommitter {
    date: String,
}

#[derive(Template)]
#[template(path = "admin_update.html")]
pub struct UpdateTmpl {
    pub current_version: String,
    pub current_commit: String,
    pub latest_version: String,
    pub latest_commit: String,
    pub latest_date: String,
    pub latest_msg: String,
    pub update_available: bool,
    pub release_url: String,
    pub message: String,
    pub user_email: String,
    pub is_admin: bool,
}

pub async fn update_page(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }
    let (user_email, _) = nav_ctx(&app, &headers);
    let cur_ver = current_version();
    let cur_commit = current_commit();

    // Try GitHub releases first, then fall back to latest commit on main
    let (latest_ver, latest_commit, latest_date, latest_msg, release_url) =
        match check_github(&app).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "github check failed");
                return page(&UpdateTmpl {
                    current_version: cur_ver,
                    current_commit: cur_commit,
                    latest_version: "unavailable".into(),
                    latest_commit: String::new(),
                    latest_date: String::new(),
                    latest_msg: format!("Could not reach GitHub: {e}"),
                    update_available: false,
                    release_url: String::new(),
                    message: String::new(),
                    user_email,
                    is_admin: true,
                });
            }
        };

    let update_available = latest_commit != cur_commit && !latest_commit.is_empty();

    page(&UpdateTmpl {
        current_version: cur_ver,
        current_commit: cur_commit,
        latest_version: latest_ver,
        latest_commit,
        latest_date,
        latest_msg,
        update_available,
        release_url,
        message: String::new(),
        user_email,
        is_admin: true,
    })
}

async fn check_github(
    app: &App,
) -> anyhow::Result<(String, String, String, String, String)> {
    // Check releases
    let resp = app
        .http
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", "Nucleus-Updater")
        .send()
        .await?;

    if resp.status().is_success() {
        let release: GhRelease = resp.json().await?;
        let tag = release.tag_name.clone();
        // For releases, we still check the latest commit to determine if update is needed
        let commit = fetch_latest_commit(app).await.unwrap_or_default();
        return Ok((
            tag,
            commit.0,
            commit.1,
            format!("Release: {}", release.html_url),
            release.html_url,
        ));
    }

    // No releases — fall back to latest commit on main
    let (sha, date, msg) = fetch_latest_commit_detail(app).await?;
    Ok((
        format!("dev ({})", &sha[..7.min(sha.len())]),
        sha,
        date,
        msg,
        format!("https://github.com/{REPO}/commits/main"),
    ))
}

async fn fetch_latest_commit(app: &App) -> anyhow::Result<(String, String)> {
    let resp = app
        .http
        .get(format!("https://api.github.com/repos/{REPO}/commits/main"))
        .header("User-Agent", "Nucleus-Updater")
        .send()
        .await?;
    let commit: GhCommit = resp.json().await?;
    let sha = commit.sha[..7.min(commit.sha.len())].to_string();
    let date = commit.commit.committer.date.clone();
    Ok((sha, date))
}

async fn fetch_latest_commit_detail(app: &App) -> anyhow::Result<(String, String, String)> {
    let resp = app
        .http
        .get(format!("https://api.github.com/repos/{REPO}/commits/main"))
        .header("User-Agent", "Nucleus-Updater")
        .send()
        .await?;
    let commit: GhCommit = resp.json().await?;
    let sha = commit.sha[..7.min(commit.sha.len())].to_string();
    let date = commit.commit.committer.date.clone();
    let msg = commit.commit.message.lines().next().unwrap_or("").to_string();
    Ok((sha, date, msg))
}

pub async fn update_perform(
    State(app): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if admin_guard(&app, &headers).is_err() {
        return Redirect::to("/login").into_response();
    }

    let cur_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot find current exe: {e}")).into_response(),
    };

    // Try to download a release binary first
    let release_resp = app
        .http
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", "Nucleus-Updater")
        .send()
        .await;

    let mut downloaded = false;
    if let Ok(resp) = release_resp {
        if resp.status().is_success() {
            if let Ok(release) = resp.json::<GhRelease>().await {
                // Find a linux x86_64 binary asset
                let asset = release.assets.iter().find(|a| {
                    a.name.contains("linux")
                        && (a.name.contains("x86_64") || a.name.contains("amd64"))
                }).or_else(|| release.assets.iter().find(|a| a.name.ends_with(".tar.gz") || a.name.ends_with(".zip")));

                if let Some(asset) = asset {
                    tracing::info!(url = %asset.browser_download_url, "downloading release binary");
                    match app.http.get(&asset.browser_download_url)
                        .header("User-Agent", "Nucleus-Updater")
                        .send().await
                    {
                        Ok(r) if r.status().is_success() => {
                            let bytes = match r.bytes().await {
                                Ok(b) => b,
                                Err(e) => return (StatusCode::BAD_GATEWAY, format!("download body failed: {e}")).into_response(),
                            };
                            if let Err(e) = write_and_replace(&cur_exe, &bytes) {
                                return (StatusCode::INTERNAL_SERVER_ERROR, format!("replace failed: {e}")).into_response();
                            }
                            downloaded = true;
                        }
                        Ok(r) => return (StatusCode::BAD_GATEWAY, format!("download HTTP {}", r.status())).into_response(),
                        Err(e) => return (StatusCode::BAD_GATEWAY, format!("download failed: {e}")).into_response(),
                    }
                }
            }
        }
    }

    // If no release binary, do a git pull + cargo build
    if !downloaded {
        tracing::info!("no release binary, falling back to git pull + cargo build");
        let repo_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let git_pull = std::process::Command::new("git")
            .args(["pull", "origin", "main"])
            .current_dir(&repo_dir)
            .output();

        match git_pull {
            Ok(o) if o.status.success() => {
                tracing::info!("git pull succeeded");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("git pull failed: {stderr}")).into_response();
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("git not available: {e}")).into_response();
            }
        }

        let cargo_build = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", "nucleus-panel", "-p", "nucleusd"])
            .current_dir(&repo_dir)
            .output();

        match cargo_build {
            Ok(o) if o.status.success() => {
                tracing::info!("cargo build succeeded");
                // Copy the new binary over the current one
                let new_bin = repo_dir.join("target/release/nucleus-panel");
                if new_bin.exists() {
                    if let Err(e) = std::fs::copy(&new_bin, &cur_exe) {
                        return (StatusCode::INTERNAL_SERVER_ERROR, format!("copy failed: {e}")).into_response();
                    }
                }
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("cargo build failed: {stderr}")).into_response();
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("cargo not available: {e}")).into_response();
            }
        }
    }

    // Restart: spawn the new binary and exit
    let args: Vec<String> = std::env::args().collect();
    let config = args.iter().find(|a| a.starts_with("--config=")).cloned()
        .or_else(|| {
            // --config <path> form
            args.windows(2).find(|w| w[0] == "--config").map(|w| w[1].clone())
        });

    let mut cmd = std::process::Command::new(&cur_exe);
    let config_path = config.clone().unwrap_or_else(|| "/etc/nucleus/panel.toml".into());
    if let Some(cfg) = config {
        cmd.arg("--config").arg(cfg);
    }
    let _ = cmd; // unused for now, we use sh script

    // Spawn a restart helper that waits 1s then execs the new binary
    let exe_str = cur_exe.to_string_lossy().to_string();
    let script = format!(
        "sleep 1 && exec {} --config {}",
        exe_str,
        config_path
    );
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    tracing::info!("update complete, restarting");
    std::process::exit(0);
}

fn write_and_replace(exe: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    let tmp = exe.with_extension("new");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, exe)?;
    Ok(())
}
