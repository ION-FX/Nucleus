use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurseForgeManifest {
    #[serde(default = "default_manifest_type", rename = "manifestType")]
    pub manifest_type: String,
    #[serde(default, rename = "manifestVersion")]
    pub manifest_version: u32,
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub minecraft: CfMinecraft,
    #[serde(default)]
    pub files: Vec<CfFileRef>,
    #[serde(default = "default_overrides")]
    pub overrides: String,
}

fn default_manifest_type() -> String {
    "minecraftModpack".into()
}

fn default_overrides() -> String {
    "overrides".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfMinecraft {
    pub version: String,
    #[serde(default, rename = "modLoaders")]
    pub mod_loaders: Vec<CfModLoader>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfModLoader {
    pub id: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CfFileRef {
    #[serde(rename = "projectID")]
    pub project_id: u64,
    #[serde(rename = "fileID")]
    pub file_id: u64,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

impl CurseForgeManifest {
    pub fn parse(json: &str) -> Result<Self> {
        let m: Self = serde_json::from_str(json).context("invalid CurseForge manifest.json")?;
        if m.manifest_type != "minecraftModpack" {
            anyhow::bail!("unsupported manifestType: {}", m.manifest_type);
        }
        Ok(m)
    }

    /// e.g. ("forge", "47.2.0") from "forge-47.2.0" or ("fabric", "0.16.9") from "fabric-0.16.9"
    pub fn primary_loader(&self) -> Option<(String, String)> {
        self.minecraft
            .mod_loaders
            .iter()
            .find(|l| l.primary)
            .or_else(|| self.minecraft.mod_loaders.first())
            .and_then(|l| {
                let mut parts = l.id.splitn(2, '-');
                Some((
                    parts.next()?.to_string(),
                    parts.next().unwrap_or("").to_string(),
                ))
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthIndex {
    pub name: String,
    #[serde(default, rename = "versionId")]
    pub version_id: String,
    #[serde(rename = "gameVersion")]
    pub game_version: String,
    #[serde(default)]
    pub files: Vec<ModrinthFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthFile {
    pub path: String,
    #[serde(alias = "downloadUrl", default)]
    pub downloads: Vec<String>,
    #[serde(default)]
    pub hashes: std::collections::BTreeMap<String, String>,
    #[serde(rename = "fileSize", default)]
    pub file_size: u64,
}

impl ModrinthIndex {
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("invalid Modrinth modrinth.index.json")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackKind {
    CurseForge,
    Modrinth,
    ServerPack,
}

/// Detect what kind of pack a zip contains based on its entry names.
pub fn detect_pack_kind(entry_names: &[String]) -> Option<PackKind> {
    for n in entry_names {
        let base = n.rsplit('/').next().unwrap_or(n);
        if base == "manifest.json" {
            return Some(PackKind::CurseForge);
        }
        if base == "modrinth.index.json" {
            return Some(PackKind::Modrinth);
        }
    }
    Some(PackKind::ServerPack)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CF_MANIFEST: &str = r#"{
        "manifestType": "minecraftModpack",
        "manifestVersion": 1,
        "name": "All of Fabric 7",
        "version": "1.1.2",
        "minecraft": {
            "version": "1.20.1",
            "modLoaders": [{"id": "fabric-0.15.11", "primary": true}]
        },
        "files": [
            {"projectID": 306612, "fileID": 5152800, "required": true},
            {"projectID": 238222, "fileID": 4833138}
        ],
        "overrides": "overrides"
    }"#;

    #[test]
    fn parses_cf_manifest() {
        let m = CurseForgeManifest::parse(CF_MANIFEST).unwrap();
        assert_eq!(m.name, "All of Fabric 7");
        assert_eq!(m.minecraft.version, "1.20.1");
        assert_eq!(m.files.len(), 2);
        assert_eq!(
            m.primary_loader().unwrap(),
            ("fabric".to_string(), "0.15.11".to_string())
        );
    }

    #[test]
    fn rejects_wrong_manifest_type() {
        let bad = CF_MANIFEST.replace("minecraftModpack", "unknownPack");
        assert!(CurseForgeManifest::parse(&bad).is_err());
    }

    #[test]
    fn parses_modrinth_index() {
        let idx = ModrinthIndex::parse(
            r#"{"name":"Fabulously Optimized","versionId":"5.5.1","gameVersion":"1.20.4","summary":"","files":[{"path":"mods/foo.jar","hashes":{"sha512":"abc"},"env":{},"downloads":["https://cdn.modrinth.com/data/x/versions/y/foo.jar"],"fileSize":123}]}"#,
        )
        .unwrap();
        assert_eq!(idx.files[0].downloads.len(), 1);
        assert_eq!(idx.files[0].path, "mods/foo.jar");
    }

    #[test]
    fn detects_kinds() {
        let cf = vec![
            "overrides/config.toml".to_string(),
            "manifest.json".to_string(),
        ];
        let mr = vec!["modrinth.index.json".to_string(), "overrides/x".to_string()];
        let sp = vec!["mods/a.jar".to_string(), "server.properties".to_string()];
        assert_eq!(detect_pack_kind(&cf), Some(PackKind::CurseForge));
        assert_eq!(detect_pack_kind(&mr), Some(PackKind::Modrinth));
        assert_eq!(detect_pack_kind(&sp), Some(PackKind::ServerPack));
    }
}
