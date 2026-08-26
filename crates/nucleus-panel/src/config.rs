use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_database")]
    pub database: PathBuf,
    /// Directory containing vendored static assets (styles.css, htmx.min.js).
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
    #[serde(default)]
    pub app_name: String,
}

fn default_bind() -> String {
    "0.0.0.0:8025".into()
}

fn default_database() -> PathBuf {
    PathBuf::from("/var/lib/nucleus/panel.db")
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("static")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            database: PathBuf::from("/var/lib/nucleus/panel.db"),
            static_dir: default_static_dir(),
            app_name: "Nucleus".into(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(toml::from_str(&raw)?),
            Err(_) => {
                tracing::warn!(%path, "config not found; using defaults");
                Ok(Self::default())
            }
        }
    }
}
