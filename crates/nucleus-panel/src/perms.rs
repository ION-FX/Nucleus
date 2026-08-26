use crate::db::Db;
use crate::models::{ServerRow, User};

pub const FLAGS: [&str; 9] = [
    "power", "console", "files", "backups", "modpacks", "ai", "schedules", "settings", "access",
];

#[derive(Clone, Default)]
pub struct Perms {
    pub flags: Vec<String>,
}

impl Perms {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
    pub fn all() -> Self {
        Self { flags: FLAGS.iter().map(|s| s.to_string()).collect() }
    }
}

/// Effective permissions of `user` on `srv`. Admins and the owner get everything.
pub fn for_server(db: &Db, user: &User, srv: &ServerRow) -> Perms {
    if user.role == "admin" || srv.owner_id == Some(user.id) {
        return Perms::all();
    }
    let row: Option<String> = db
        .with(|c| {
            let mut stmt =
                c.prepare("SELECT perms FROM user_servers WHERE user_id=?1 AND server_id=?2")?;
            let mut rows = stmt.query(rusqlite::params![user.id, srv.id])?;
            Ok(if let Some(r) = rows.next()? { Some(r.get(0)?) } else { None })
        })
        .ok()
        .flatten();
    Perms {
        flags: row
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

pub fn allowed(db: &Db, user: &User, srv: &ServerRow, flag: &str) -> bool {
    for_server(db, user, srv).has(flag)
}

pub fn is_owner_or_admin(db: &Db, user: &User, srv: &ServerRow) -> bool {
    user.role == "admin" || srv.owner_id == Some(user.id)
}

/// True if the user can see the server at all (owner, admin, or any membership row).
pub fn has_any_access(db: &Db, user: &User, srv: &ServerRow) -> bool {
    if user.role == "admin" || srv.owner_id == Some(user.id) {
        return true;
    }
    db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT 1 FROM user_servers WHERE user_id=?1 AND server_id=?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![user.id, srv.id])?;
        Ok(rows.next()?.is_some())
    })
    .unwrap_or(false)
}

pub fn record(db: &Db, email: &str, action: &str, target: &str, detail: &str) {
    let _ = db.with(|c| {
        c.execute(
            "INSERT INTO activity (ts, email, action, target, detail) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![chrono::Utc::now().timestamp(), email, action, target, detail],
        )?;
        Ok(())
    });
}
