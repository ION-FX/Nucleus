use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    Start,
    Stop,
    Kill,
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
    #[serde(default = "default_proto")]
    pub proto: String,
}

fn default_proto() -> String {
    "tcp".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub mem_mb: u64,
    #[serde(default)]
    pub cpu_cores: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            mem_mb: 2048,
            cpu_cores: 2.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServerRequest {
    pub id: String,
    pub name: String,
    pub image: String,
    pub startup: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<PortMapping>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub stop_command: Option<String>,
    #[serde(default)]
    pub accept_eula: bool,
    /// Shell script executed in a sidecar container right after creation.
    #[serde(default)]
    pub install_script: Option<String>,
    /// Image for the sidecar (defaults to the server image).
    #[serde(default)]
    pub installer_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerRequest {
    pub action: PowerAction,
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub id: String,
    pub name: String,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mode: u32,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallStatus {
    pub state: String,
    pub lines: Vec<String>,
}
