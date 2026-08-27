use crate::config::Config;
use bollard::Docker;
use dashmap::DashMap;
use nucleus_core::{CreateServerRequest, ServerStatus};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub const RING_CAPACITY: usize = 2000;

/// Writer to a running container's process stdin (from Docker attach).
pub type StdinSink = std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>;

#[derive(Debug, Clone)]
pub enum LogEvent {
    Data(String),
    Exit(i64),
}

/// Event emitted when a container process terminates.
/// Remove ANSI escape sequences (colors, cursor movement) from a log line.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: parameters/intermediates are < 0x40; final byte is 0x40-0x7E
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: swallow until BEL or ST
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\u{7}' { break; }
                        if n == '\u{1b}' { chars.next(); break; }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    out
}

pub fn decode_exit_event(code: i64) -> LogEvent {
    LogEvent::Exit(code)
}

pub struct ServerRuntime {
    pub spec: CreateServerRequest,
    pub log_tx: broadcast::Sender<LogEvent>,
    pub ring: Mutex<VecDeque<String>>,
    pub stdin: Mutex<Option<StdinSink>>,
    pub running: AtomicBool,
    pub exit_code: Mutex<Option<i64>>,
    pub container_id: Mutex<Option<String>>,
}

impl ServerRuntime {
    pub fn new(spec: CreateServerRequest) -> Self {
        let (log_tx, _) = broadcast::channel(1024);
        Self {
            spec,
            log_tx,
            ring: Mutex::new(VecDeque::new()),
            stdin: Mutex::new(None),
            running: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            container_id: Mutex::new(None),
        }
    }

    pub fn push_log(&self, raw: &str) {
        let line = strip_ansi(raw);
        let mut ring = self.ring.lock().unwrap();
        if ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(line.clone());
        drop(ring);
        let _ = self.log_tx.send(LogEvent::Data(line));
    }

    pub fn recent_logs(&self, tail: usize) -> Vec<String> {
        self.ring
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(tail)
            .rev()
            .cloned()
            .collect()
    }

    pub fn status(&self) -> ServerStatus {
        ServerStatus {
            id: self.spec.id.clone(),
            name: self.spec.name.clone(),
            running: self.running.load(Ordering::Relaxed),
            exit_code: *self.exit_code.lock().unwrap(),
            container_id: self.container_id.lock().unwrap().clone(),
        }
    }

    pub fn server_dir(&self, cfg: &Config) -> PathBuf {
        cfg.servers_dir().join(&self.spec.id)
    }
}

/// Progress tracker for an asynchronous modpack/pack install job.
#[derive(Default)]
pub struct InstallJob {
    pub lines: Mutex<Vec<String>>,
    pub state: Mutex<String>,
}

pub struct AppState {
    pub cfg: Config,
    pub docker: Docker,
    pub http: reqwest::Client,
    pub servers: DashMap<String, Arc<ServerRuntime>>,
    /// Per-server AI incident lock so auto-heal never double-runs.
    pub ai_busy: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Active/last install job per server.
    pub installs: DashMap<String, Arc<InstallJob>>,
    /// Container exit events, consumed by the auto-heal worker.
    pub exit_tx: tokio::sync::mpsc::UnboundedSender<(String, i64)>,
    /// Per-server SFTP passwords (server_id -> password).
    pub sftp_creds: std::sync::RwLock<BTreeMap<String, String>>,
    /// Last docker CPU sample for delta computation.
    pub stats_prev: DashMap<String, (u64, u64)>,
    /// Last network totals for rate computation.
    pub net_prev: DashMap<String, NetPrev>,
    /// When the daemon process started (for uptime reporting).
    pub started: std::time::Instant,
}

#[derive(Clone, Copy)]
pub struct NetPrev {
    pub rx: u64,
    pub tx: u64,
    pub at: std::time::Instant,
}

impl InstallJob {
    pub fn log(&self, line: impl Into<String>) {
        let mut l = self.lines.lock().unwrap();
        if l.len() > 500 {
            l.remove(0);
        }
        l.push(line.into());
    }

    pub fn set_state(&self, s: &str) {
        *self.state.lock().unwrap() = s.to_string();
    }
}

impl AppState {
    pub fn new(
        cfg: Config,
        docker: Docker,
        exit_tx: tokio::sync::mpsc::UnboundedSender<(String, i64)>,
    ) -> Self {
        let sftp_path = cfg.data_dir.join("sftp.json");
        let creds: BTreeMap<String, String> = std::fs::read_to_string(&sftp_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            cfg,
            docker,
            http: reqwest::Client::builder()
                .user_agent("nucleusd/0.1")
                .build()
                .expect("http client"),
            servers: DashMap::new(),
            ai_busy: DashMap::new(),
            installs: DashMap::new(),
            exit_tx,
            sftp_creds: std::sync::RwLock::new(creds),
            stats_prev: DashMap::new(),
            net_prev: DashMap::new(),
            started: std::time::Instant::now(),
        }
    }

    fn save_sftp_creds(&self) {
        let path = self.cfg.data_dir.join("sftp.json");
        if let Ok(json) = serde_json::to_string_pretty(&*self.sftp_creds.read().unwrap()) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Fetch the stored SFTP password for a server, generating one lazily.
    pub fn sftp_password(&self, id: &str) -> String {
        let mut creds = self.sftp_creds.write().unwrap();
        creds
            .entry(id.to_string())
            .or_insert_with(crate::auth::generate_password)
            .clone()
    }

    pub fn reset_sftp_password(&self, id: &str) -> String {
        let pw = crate::auth::generate_password();
        self.sftp_creds.write().unwrap().insert(id.to_string(), pw.clone());
        self.save_sftp_creds();
        pw
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Arc<ServerRuntime>> {
        self.servers
            .get(id)
            .map(|r| r.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown server {id}"))
    }
}

/// Persist the set of known server specs so the daemon restores them on restart.
pub fn save_registry(cfg: &Config, servers: &DashMap<String, Arc<ServerRuntime>>) {
    let specs: BTreeMap<String, CreateServerRequest> = servers
        .iter()
        .map(|e| (e.key().clone(), e.value().spec.clone()))
        .collect();
    let path = cfg.data_dir.join("servers.json");
    if let Ok(json) = serde_json::to_string_pretty(&specs) {
        let _ = std::fs::create_dir_all(&cfg.data_dir);
        let _ = std::fs::write(path, json);
    }
}

pub fn load_registry(cfg: &Config) -> BTreeMap<String, CreateServerRequest> {
    let path = cfg.data_dir.join("servers.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_colors_and_cursor_codes() {
        assert_eq!(strip_ansi("\u{1b}[32m[17:50:02] INFO\u{1b}[0m"), "[17:50:02] INFO");
        assert_eq!(strip_ansi(">\u{1b}[Ktext"), ">text");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}plain"), "plain");
        assert_eq!(strip_ansi("clean"), "clean");
    }
}
