use nucleus_core::Egg;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub role: String,
    pub totp_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub url: String,
    #[allow(dead_code)]
    pub token: String,
    pub alias: String,
    /// Accept self-signed daemon certificates (trust-on-first-use alternative).
    pub tls_insecure: bool,
    /// Optional PEM CA bundle for the daemon's certificate.
    pub tls_ca_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EggRow {
    pub slug: String,
    pub name: String,
    pub egg: Egg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRow {
    pub id: String,
    pub name: String,
    pub node_id: String,
    pub egg_slug: Option<String>,
    pub image: String,
    pub startup: String,
    pub env_json: String,
    pub ports_json: String,
    pub mem_mb: u64,
    pub cpu: f64,
    pub disk_mb: u64,
    pub pids_limit: i64,
    pub tags: String,
    pub stop_command: Option<String>,
    pub accept_eula: bool,
    pub owner_id: Option<i64>,
    /// Keep at most this many backups (0 = unlimited).
    pub backup_retention: u32,
    /// "auto" (None), "on", "off" — quiesce before archiving.
    pub backup_quiesce: Option<String>,
}

impl ServerRow {
    /// "on"/"off"/"auto" → the daemon's Option<bool> (None = auto-detect).
    pub fn quiesce_flag(&self) -> Option<bool> {
        match self.backup_quiesce.as_deref() {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ => None,
        }
    }
}
