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
