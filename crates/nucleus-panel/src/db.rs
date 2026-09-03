use anyhow::{Context, Result};
use rusqlite::Connection;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path).with_context(|| format!("opening database {path}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    pub fn migrate(&self) -> Result<()> {
        self.with(|c| {
            c.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    email TEXT UNIQUE NOT NULL,
                    password_hash TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'user',
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS sessions (
                    token TEXT PRIMARY KEY,
                    user_id INTEGER NOT NULL REFERENCES users(id),
                    expires_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS nodes (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    url TEXT NOT NULL,
                    token TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS eggs (
                    slug TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    json TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS user_servers (
                    user_id INTEGER NOT NULL REFERENCES users(id),
                    server_id TEXT NOT NULL REFERENCES servers(id),
                    perms TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (user_id, server_id)
                );
                CREATE TABLE IF NOT EXISTS activity (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts INTEGER NOT NULL,
                    email TEXT NOT NULL,
                    action TEXT NOT NULL,
                    target TEXT NOT NULL DEFAULT '',
                    detail TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_activity_ts ON activity(ts DESC);
                CREATE TABLE IF NOT EXISTS api_keys (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id INTEGER NOT NULL REFERENCES users(id),
                    name TEXT NOT NULL,
                    key_hash TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    last_used INTEGER
                );
                CREATE TABLE IF NOT EXISTS invites (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    token TEXT UNIQUE NOT NULL,
                    email TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'user',
                    invited_by INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    used_at INTEGER
                );
                CREATE TABLE IF NOT EXISTS pending_2fa (
                    token TEXT PRIMARY KEY,
                    user_id INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS servers (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    node_id TEXT NOT NULL REFERENCES nodes(id),
                    egg_slug TEXT,
                    image TEXT NOT NULL,
                    startup TEXT NOT NULL,
                    env_json TEXT NOT NULL DEFAULT '{}',
                    ports_json TEXT NOT NULL DEFAULT '[]',
                    mem_mb INTEGER NOT NULL DEFAULT 2048,
                    cpu REAL NOT NULL DEFAULT 2.0,
                    stop_command TEXT,
                    accept_eula INTEGER NOT NULL DEFAULT 0,
                    owner_id INTEGER,
                    created_at INTEGER NOT NULL
                );
                "#,
            )?;

            // lightweight column migrations
            let has_alias: bool = c
                .prepare("PRAGMA table_info(nodes)")?
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .any(|name| name == "alias");
            if !has_alias {
                c.execute("ALTER TABLE nodes ADD COLUMN alias TEXT NOT NULL DEFAULT ''", [])?;
            }
            for col in [
                "ALTER TABLE users ADD COLUMN totp_secret TEXT",
                "ALTER TABLE users ADD COLUMN totp_enabled INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE servers ADD COLUMN disk_mb INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE servers ADD COLUMN pids_limit INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE servers ADD COLUMN tags TEXT NOT NULL DEFAULT ''",
                "ALTER TABLE servers ADD COLUMN backup_retention INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE servers ADD COLUMN backup_quiesce TEXT",
            ] {
                let _ = c.execute(col, []); // ignore if already present
            }
            // settings table for global defaults
            c.execute(
                "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                [],
            )?;
            Ok(())
        })
    }
}
