use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    pub enabled: bool,
    pub provider: AiProvider,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Literal key or `env:VAR_NAME` to read from environment.
    pub api_key: String,
    pub model: String,
    /// Watch containers and auto-run the agent when a server crashes (non-zero exit).
    #[serde(default)]
    pub auto_heal: bool,
    /// Allow the agent to issue power actions on its own; otherwise it only reports.
    #[serde(default = "default_true")]
    pub allow_power_actions: bool,
    #[serde(default = "default_max_rounds")]
    pub max_tool_rounds: u32,
}

fn default_true() -> bool {
    true
}

fn default_max_rounds() -> u32 {
    6
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: AiProvider::OpenAi,
            base_url: None,
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            auto_heal: false,
            allow_power_actions: true,
            max_tool_rounds: default_max_rounds(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SftpConfig {
    #[serde(default = "default_sftp_enabled")]
    pub enabled: bool,
    #[serde(default = "default_sftp_bind")]
    pub bind: String,
}

fn default_sftp_enabled() -> bool {
    true
}

fn default_sftp_bind() -> String {
    "0.0.0.0:2022".into()
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self { enabled: true, bind: default_sftp_bind() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Shared secret daemons require from the panel (`Authorization: Bearer <token>`).
    pub token: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// CurseForge API key (or `env:CF_API_KEY`); optional but improves mod resolution.
    #[serde(default)]
    pub curseforge_api_key: Option<String>,
    #[serde(default)]
    pub ai: AiConfig,
    #[serde(default)]
    pub sftp: SftpConfig,
}

fn default_bind() -> String {
    "0.0.0.0:8033".into()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/nucleus")
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading config {}", path))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config")?;
        Ok(cfg)
    }

    pub fn resolve_secret(secret: &Option<String>) -> Option<String> {
        match secret {
            Some(s) if s.starts_with("env:") => {
                std::env::var(&s[4..]).ok().filter(|v| !v.is_empty())
            }
            Some(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    pub fn cf_api_key(&self) -> Option<String> {
        Self::resolve_secret(&self.curseforge_api_key)
    }

    pub fn servers_dir(&self) -> PathBuf {
        self.data_dir.join("servers")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
}
