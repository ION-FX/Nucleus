use nucleus_core::Egg;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub email: String,
    #[allow(dead_code)]
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub url: String,
    #[allow(dead_code)]
    pub token: String,
    pub alias: String,
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
    pub stop_command: Option<String>,
    pub accept_eula: bool,
    pub owner_id: Option<i64>,
}
