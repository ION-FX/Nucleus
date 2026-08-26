use crate::state::AppState;

/// Extract a gzip-compressed tar archive into `dir` (overwriting).
pub async fn extract_tar_gz(dir: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Read;
    let dir = dir.to_path_buf();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let cursor = std::io::Cursor::new(bytes);
        let dec = flate2::read::GzDecoder::new(cursor);
        let mut tar = tar::Archive::new(dec);
        tar.unpack(&dir).context("extracting archive")?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("join error {e}"))?
}

/// Remove every entry inside `dir` (but keep the directory itself).
pub async fn clear_dir(dir: &std::path::Path) -> Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if dir.exists() {
            for ent in std::fs::read_dir(&dir)? {
                let ent = ent?;
                let p = ent.path();
                if ent.file_type()?.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                } else {
                    std::fs::remove_file(&p)?;
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("join error {e}"))?;
    Ok(())
}

pub async fn restore_backup(state: Arc<AppState>, id: String, bid: String) -> Result<()> {
    if !bid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("invalid backup id");
    }
    let rt = state.get(&id)?;
    if rt.running.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = crate::docker::power(state.clone(), &id, nucleus_core::PowerAction::Kill, None).await;
    }
    let name = format!("nucleus-{id}");
    let _ = state
        .docker
        .remove_container(
            &name,
            Some(bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    let dir = rt.server_dir(&state.cfg);
    tokio::fs::create_dir_all(&dir).await.ok();
    clear_dir(&dir).await?;
    let dest = backups_root(&state, &id).join(format!("{bid}.tar.gz"));
    let bytes = tokio::fs::read(&dest).await.context("backup not found")?;
    extract_tar_gz(&dir, &bytes).await?;
    rt.push_log("[nucleus] backup restored; start the server to apply");
    Ok(())
}

use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Serialize)]
pub struct BackupInfo {
    pub id: String,
    pub size: u64,
    pub created_at: i64,
}

pub fn backups_root(state: &AppState, id: &str) -> PathBuf {
    state.cfg.backups_dir().join(id)
}

pub async fn create_backup(state: Arc<AppState>, id: String) -> Result<BackupInfo> {
    let rt = state.get(&id)?;
    let src = rt.server_dir(&state.cfg);
    let dest_dir = backups_root(&state, &id);
    tokio::fs::create_dir_all(&dest_dir).await?;

    let backup_id = format!("{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let dest = dest_dir.join(format!("{backup_id}.tar.gz"));
    let src2 = src.clone();
    let dest2 = dest.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::File::create(&dest2).context("creating archive")?;
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        tar.follow_symlinks(false);
        tar.append_dir_all(".", &src2)
            .context("archiving server data")?;
        tar.into_inner()?.finish()?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("join error {e}"))??;

    let md = std::fs::metadata(&dest)?;
    Ok(BackupInfo {
        id: backup_id,
        size: md.len(),
        created_at: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    })
}

pub async fn list_backups(state: Arc<AppState>, id: String) -> Result<Vec<BackupInfo>> {
    let dir = backups_root(&state, &id);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) == Some("gz") {
            let md = ent.metadata()?;
            let fname = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push(BackupInfo {
                id: fname
                    .strip_suffix(".tar.gz")
                    .map(str::to_owned)
                    .unwrap_or(fname),
                size: md.len(),
                created_at: md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

pub async fn download_backup(state: Arc<AppState>, id: String, bid: String) -> Result<Response> {
    if !bid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("invalid backup id");
    }
    let path = backups_root(&state, &id).join(format!("{bid}.tar.gz"));
    let bytes = tokio::fs::read(path).await.context("backup not found")?;
    Ok(Response::builder()
        .header("content-type", "application/gzip")
        .header("x-nucleus-filename", format!("{bid}.tar.gz"))
        .body(Body::from(bytes))
        .unwrap())
}

pub async fn delete_backup(state: Arc<AppState>, id: String, bid: String) -> Result<StatusCode> {
    if !bid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("invalid backup id");
    }
    let path = backups_root(&state, &id).join(format!("{bid}.tar.gz"));
    tokio::fs::remove_file(path)
        .await
        .context("backup not found")?;
    Ok(StatusCode::NO_CONTENT)
}
