use crate::models::User;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, SaltString},
    Argon2, PasswordVerifier,
};
use axum::http::HeaderMap;
use chrono::Utc;
use rand::Rng;
use urlencoding;

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
                r#"SELECT u.id, u.email, u.role, COALESCE(u.totp_enabled, 0)
                   FROM users u
                   JOIN sessions s ON s.user_id = u.id
                   WHERE s.token = ?1 AND s.expires_at > ?2"#,
            )?;
            let mut rows = stmt.query(rusqlite::params![token, Utc::now().timestamp()])?;
            if let Some(row) = rows.next()? {
                Ok(Some(User {
                    id: row.get(0)?,
                    email: row.get(1)?,
                    role: row.get(2)?,
                    totp_enabled: row.get::<_, i64>(3)? != 0,
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

    pub const PENDING_2FA_COOKIE: &'static str = "nucleus_2fa";
    pub fn pending_cookie(token: &str) -> String {
        format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600",
            Self::PENDING_2FA_COOKIE
        )
    }
    pub fn clear_pending_cookie() -> String {
        format!("{}=deleted; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", Self::PENDING_2FA_COOKIE)
    }
}

// ---------- TOTP (2FA) ----------

/// Generate a fresh base32 TOTP secret (20 random bytes).
pub fn totp_new_secret() -> String {
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill(&mut bytes);
    base32::encode(base32::Alphabet::RFC4648 { padding: false }, &bytes)
}

/// Verify a 6-digit TOTP code against a stored base32 secret.
pub fn totp_verify(secret_base32: &str, code: &str) -> bool {
    let Some(bytes) = base32::decode(base32::Alphabet::RFC4648 { padding: false }, secret_base32) else {
        return false;
    };
    let totp = totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
    );
    totp.check_current(code.trim()).unwrap_or(false)
}

/// Build an otpauth:// URI for QR code generation.
pub fn totp_uri(secret_base32: &str, email: &str, issuer: &str) -> String {
    let label = urlencoding::encode(email);
    format!(
        "otpauth://totp/{label}?secret={secret_base32}&issuer={issuer}&algorithm=SHA1&digits=6&period=30"
    )
}

// ---------- API keys ----------

pub fn gen_api_key() -> (String, String) {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill(&mut bytes);
    let raw = format!("nuc_{}", base32::encode(base32::Alphabet::RFC4648 { padding: false }, &bytes));
    (raw.clone(), api_key_hash(&raw))
}

pub fn api_key_hash(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    let out = h.finalize();
    format!("sha256:{out:x}")
}

pub fn user_for_api_key(db: &crate::db::Db, headers: &HeaderMap) -> Option<User> {
    let token = crate::routes::proxy::bearer_token(headers)?;
    db.with(|c| {
        let uid: Option<i64> = c
            .query_row(
                "SELECT user_id FROM api_keys WHERE key_hash = ?1",
                rusqlite::params![api_key_hash(&token)],
                |r| r.get(0),
            )
            .ok();
        let Some(uid) = uid else { return Ok(None) };
        let _ = c.execute(
            "UPDATE api_keys SET last_used = ?1 WHERE key_hash = ?2",
            rusqlite::params![chrono::Utc::now().timestamp(), api_key_hash(&token)],
        );
        let mut stmt = c.prepare(
            "SELECT id, email, role, COALESCE(totp_enabled,0) FROM users WHERE id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![uid])?;
        Ok(if let Some(row) = rows.next()? {
            Some(User {
                id: row.get(0)?,
                email: row.get(1)?,
                role: row.get(2)?,
                totp_enabled: row.get::<_, i64>(3)? != 0,
            })
        } else {
            None
        })
    })
    .ok()
    .flatten()
}

// ---------- pending 2FA ----------

pub fn create_pending_2fa(db: &crate::db::Db, user_id: i64) -> String {
    let token = new_token();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).timestamp();
    let _ = db.with(|c| {
        c.execute(
            "INSERT INTO pending_2fa (token, user_id, expires_at) VALUES (?1,?2,?3)",
            rusqlite::params![token, user_id, expires],
        )?;
        Ok(())
    });
    token
}

pub fn consume_pending_2fa(db: &crate::db::Db, token: &str) -> Option<i64> {
    let uid: Option<i64> = db
        .with(|c| {
            let now = chrono::Utc::now().timestamp();
            let uid: Option<i64> = c
                .query_row(
                    "SELECT user_id FROM pending_2fa WHERE token = ?1 AND expires_at > ?2",
                    rusqlite::params![token, now],
                    |r| r.get(0),
                )
                .ok();
            if uid.is_some() {
                c.execute("DELETE FROM pending_2fa WHERE token = ?1", rusqlite::params![token])?;
            }
            Ok(uid)
        })
        .ok()
        .flatten();
    uid
}
