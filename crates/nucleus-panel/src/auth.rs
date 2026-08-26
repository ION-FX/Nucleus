use crate::models::User;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, SaltString},
    Argon2, PasswordVerifier,
};
use axum::http::HeaderMap;
use chrono::Utc;
use rand::Rng;

pub const SESSION_COOKIE: &str = "nucleus_session";
const SESSION_TTL_HOURS: i64 = 24 * 14;

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

pub fn new_token() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn cookie_from(headers: &HeaderMap, name: &str) -> Option<String> {
    for v in headers.get_all(axum::http::header::COOKIE) {
        if let Ok(s) = v.to_str() {
            for pair in s.split(';') {
                let mut kv = pair.trim().splitn(2, '=');
                if kv.next()? == name {
                    return kv.next().map(str::to_owned);
                }
            }
        }
    }
    None
}

pub struct Sessions;

impl Sessions {
    pub fn create(db: &crate::db::Db, user_id: i64) -> anyhow::Result<String> {
        let token = new_token();
        let expires = (Utc::now() + chrono::Duration::hours(SESSION_TTL_HOURS)).timestamp();
        db.with(|c| {
            c.execute(
                "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![token, user_id, expires],
            )?;
            c.execute(
                "DELETE FROM sessions WHERE expires_at < ?1",
                rusqlite::params![Utc::now().timestamp()],
            )?;
            Ok(())
        })?;
        Ok(token)
    }

    pub fn user_for(db: &crate::db::Db, headers: &HeaderMap) -> Option<User> {
        let token = cookie_from(headers, SESSION_COOKIE)?;
        db.with(|c| {
            let mut stmt = c.prepare(
                r#"SELECT u.id, u.email, u.role FROM users u
                   JOIN sessions s ON s.user_id = u.id
                   WHERE s.token = ?1 AND s.expires_at > ?2"#,
            )?;
            let mut rows = stmt.query(rusqlite::params![token, Utc::now().timestamp()])?;
            if let Some(row) = rows.next()? {
                Ok(Some(User {
                    id: row.get(0)?,
                    email: row.get(1)?,
                    role: row.get(2)?,
                }))
            } else {
                Ok(None)
            }
        })
        .ok()
        .flatten()
    }

    pub fn destroy(db: &crate::db::Db, headers: &HeaderMap) {
        if let Some(token) = cookie_from(headers, SESSION_COOKIE) {
            let _ = db.with(|c| {
                c.execute(
                    "DELETE FROM sessions WHERE token = ?1",
                    rusqlite::params![token],
                )?;
                Ok(())
            });
        }
    }

    pub fn session_cookie(token: &str) -> String {
        format!(
            "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
            SESSION_TTL_HOURS * 3600
        )
    }

    pub fn clear_cookie() -> String {
        format!("{SESSION_COOKIE}=deleted; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
    }
}
