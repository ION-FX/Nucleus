use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use nucleus_core::FileEntry;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn server_root(state: &AppState, id: &str) -> Result<PathBuf> {
    let rt = state.get(id)?;
    let dir = rt.server_dir(&state.cfg);
    std::fs::create_dir_all(&dir).ok();
    Ok(dir)
}

/// Resolve a user-supplied relative path safely under `root`.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    if rel.contains('\0') {
        bail!("invalid path");
    }
    let rel_trim = rel.trim_start_matches('/');
    let mut depth = 0i32;
    for comp in Path::new(rel_trim).components() {
        match comp {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    bail!("path escapes root");
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            _ => bail!("invalid path component"),
        }
    }
    let p = root.join(rel_trim);
    ensure_inside(root, &p)?;
    Ok(p)
}

/// Refuse any target whose existing ancestor resolves outside `root` (symlinks, `..`).
fn ensure_inside(root: &Path, target: &Path) -> Result<()> {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut probe = target.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p.to_path_buf(),
            None => break,
        }
    }
    if probe.exists() {
        let canon = probe.canonicalize()?;
        if !canon.starts_with(&root_canon) {
            bail!("path escapes root");
        }
    }
    Ok(())
}

fn entry_from_dirent(root: &Path, ent: &std::fs::DirEntry) -> Option<FileEntry> {
    use std::os::unix::fs::PermissionsExt;
    let md = ent.metadata().ok()?;
    let name = ent.file_name().to_string_lossy().to_string();
    Some(FileEntry {
        path: PathBuf::from("/")
            .join(ent.path().strip_prefix(root).ok()?)
            .to_string_lossy()
            .replace('\\', "/"),
        is_dir: md.is_dir(),
        size: md.len(),
        mode: (md.permissions().mode() & 0o777) as u32,
        name,
        modified_at: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    })
}

pub async fn list_files(
    state: Arc<AppState>,
    id: String,
    path: Option<String>,
) -> Result<Vec<FileEntry>, ApiError> {
    list_files_inner(&state, &id, path).await.map_err(ApiError)
}

pub async fn list_files_inner(
    state: &Arc<AppState>,
    id: &str,
    path: Option<String>,
) -> anyhow::Result<Vec<FileEntry>> {
    let root = server_root(state, id)?;
    let dir = safe_join(&root, path.as_deref().unwrap_or(""))?;
    if !dir.exists() {
        return Err(anyhow!("no such directory"));
    }
    let mut out = Vec::new();
    for ent in std::fs::read_dir(&dir).with_context(|| "reading dir")? {
        if let Some(e) = ent.ok().and_then(|e| entry_from_dirent(&dir, &e)) {
            out.push(e);
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(out)
}

pub async fn read_file(
    state: Arc<AppState>,
    id: String,
    path: String,
) -> Result<Response, ApiError> {
    let root = server_root(&state, &id)?;
    let file = safe_join(&root, &path)?;
    if file.is_dir() {
        return Err(anyhow!("is a directory").into());
    }
    let bytes = tokio::fs::read(&file)
        .await
        .with_context(|| format!("reading {path}"))?;
    Ok(Response::builder()
        .header("content-type", "application/octet-stream")
        .header(
            "x-nucleus-filename",
            urlencoding::encode(&file_name_of(&path)).to_string(),
        )
        .body(Body::from(bytes))
        .unwrap())
}

const MAX_WRITE: usize = 512 * 1024 * 1024;

pub async fn write_file(
    state: Arc<AppState>,
    id: String,
    path: String,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    if body.len() > MAX_WRITE {
        return Err(anyhow!("file too large").into());
    }
    let root = server_root(&state, &id)?;
    let target = safe_join(&root, &path)?;
    if target.is_dir() {
        return Err(anyhow!("is a directory").into());
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(&target, &body)
        .await
        .with_context(|| format!("writing {path}"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct MkdirReq {
    pub path: String,
}

#[derive(Deserialize)]
pub struct DeleteReq {
    pub path: String,
}

#[derive(Deserialize)]
pub struct RenameReq {
    pub from: String,
    pub to: String,
}

#[derive(Deserialize)]
pub struct FetchReq {
    pub url: String,
    pub path: String,
}

const MAX_FETCH: u64 = 4 * 1024 * 1024 * 1024;

/// Download a remote file directly into the server data dir.
pub async fn fetch_file(
    state: Arc<AppState>,
    id: String,
    req: FetchReq,
) -> Result<StatusCode, ApiError> {
    let root = server_root(&state, &id)?;
    let target = safe_join(&root, &req.path)?;
    if target.is_dir() {
        return Err(anyhow!("target is a directory").into());
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let resp = state
        .http
        .get(&req.url)
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
        .map_err(anyhow::Error::from)?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()).into());
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_FETCH {
            return Err(anyhow!("file too large").into());
        }
    }

    let tmp = target.with_extension("nucleus-partial");
    let mut file = tokio::io::BufWriter::new(tokio::fs::File::create(&tmp).await?);
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(anyhow::Error::from)?;
        written += chunk.len() as u64;
        if written > MAX_FETCH {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(anyhow!("file too large").into());
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, &target).await?;
    tracing::info!(server = %id, path = %req.path, bytes = written, "fetched file");
    Ok(StatusCode::CREATED)
}

pub async fn mkdir(
    state: Arc<AppState>,
    id: String,
    req: MkdirReq,
) -> Result<StatusCode, ApiError> {
    let root = server_root(&state, &id)?;
    let dir = safe_join(&root, &req.path)?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete(
    state: Arc<AppState>,
    id: String,
    req: DeleteReq,
) -> Result<StatusCode, ApiError> {
    let root = server_root(&state, &id)?;
    let target = safe_join(&root, &req.path)?;
    if target == root {
        return Err(anyhow!("cannot delete server root").into());
    }
    let res = if target.is_dir() {
        tokio::fs::remove_dir_all(&target).await
    } else {
        tokio::fs::remove_file(&target).await
    };
    if let Err(e) = res {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            // Game containers run as root, so files they wrote are
            // root-owned and the daemon user can't unlink them. Remove via a
            // throwaway container bind-mounted over the server dir.
            let rel = target
                .strip_prefix(&root)
                .unwrap_or(Path::new(&req.path))
                .to_string_lossy()
                .replace('\\', "/");
            docker_remove(&state, &root, &rel)
                .await
                .with_context(|| format!("deleting root-owned {}", req.path))?;
            return Ok(StatusCode::NO_CONTENT);
        }
        return Err(anyhow::Error::from(e)
            .context(format!("deleting {}", req.path))
            .into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Remove `rel` under `root` (or everything under `root` when `rel` is empty)
/// using a busybox container. The container is labeled `nucleus.oneoff` so
/// boot reconciliation reaps it if the daemon dies mid-delete.
pub(crate) async fn docker_remove(state: &AppState, root: &Path, rel: &str) -> Result<()> {
    const IMG: &str = "busybox:latest";
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let name = format!("nucleus-fsop-{nanos}");
    if !crate::docker::image_exists(&state.docker, IMG).await {
        crate::docker::pull_image(&state.docker, IMG).await?;
    }
    let (entrypoint, cmd): (Vec<String>, Vec<String>) = if rel.is_empty() {
        (
            vec!["/bin/sh".into(), "-c".into()],
            vec![format!("rm -rf -- /data/* /data/.[!.]*")],
        )
    } else {
        // Pass the path as an argv element: no shell quoting pitfalls.
        (vec!["rm".into()], vec!["-rf".into(), "--".into(), format!("/data/{rel}")])
    };
    let cfg = bollard::container::Config {
        image: Some(IMG.into()),
        entrypoint: Some(entrypoint),
        cmd: Some(cmd),
        host_config: Some(bollard::secret::HostConfig {
            binds: Some(vec![format!("{}:/data", root.display())]),
            ..Default::default()
        }),
        labels: Some(
            [("nucleus.oneoff", "fsop")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..Default::default()
    };
    state
        .docker
        .create_container(
            Some(bollard::container::CreateContainerOptions {
                name: name.clone(),
                platform: None,
            }),
            cfg,
        )
        .await
        .context("creating fsop container")?;
    state
        .docker
        .start_container(&name, None::<bollard::container::StartContainerOptions<String>>)
        .await?;
    use futures_util::StreamExt;
    let mut wait = state
        .docker
        .wait_container(&name, None::<bollard::container::WaitContainerOptions<String>>);
    while let Some(st) = wait.next().await {
        st.map_err(|e| anyhow!("fsop container failed: {e}"))?;
    }
    let _ = state
        .docker
        .remove_container(&name, Some(bollard::container::RemoveContainerOptions {
            force: true,
            ..Default::default()
        }))
        .await;
    Ok(())
}

pub async fn rename(
    state: Arc<AppState>,
    id: String,
    req: RenameReq,
) -> Result<StatusCode, ApiError> {
    let root = server_root(&state, &id)?;
    let from = safe_join(&root, &req.from)?;
    let to = safe_join(&root, &req.to)?;
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::rename(from, to)
        .await
        .with_context(|| format!("renaming {} -> {}", req.from, req.to))?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn file_name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

#[derive(Deserialize)]
pub struct ArchiveReq {
    pub path: String,
    #[serde(default)]
    pub action: String,
}

pub async fn archive(
    state: Arc<AppState>,
    id: String,
    req: ArchiveReq,
) -> Result<StatusCode, ApiError> {
    let root = server_root(&state, &id)?;
    let target = safe_join(&root, &req.path)?;
    ensure_inside(&root, &target)?;
    if !target.exists() {
        return Err(anyhow::anyhow!("path not found: {}", req.path).into());
    }
    let archive_path = if target.is_dir() {
        let parent = target.parent().unwrap_or(&root);
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        parent.join(format!("{}.tar.gz", name))
    } else {
        let parent = target.parent().unwrap_or(&root);
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        parent.join(format!("{}.tar.gz", name))
    };
    let tar_gz = std::fs::File::create(&archive_path)
        .map_err(|e| anyhow::anyhow!("create archive: {e}"))?;
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    if target.is_dir() {
        tar.append_dir_all(".", &target)
            .map_err(|e| anyhow::anyhow!("tar: {e}"))?;
    } else {
        let name = target.file_name().unwrap_or_default().to_string_lossy().to_string();
        tar.append_file(&name, &mut std::fs::File::open(&target).map_err(|e| anyhow::anyhow!("open: {e}"))?)
            .map_err(|e| anyhow::anyhow!("tar file: {e}"))?;
    }
    tar.finish().map_err(|e| anyhow::anyhow!("finish tar: {e}"))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn extract(
    state: Arc<AppState>,
    id: String,
    req: ArchiveReq,
) -> Result<StatusCode, ApiError> {
    let root = server_root(&state, &id)?;
    let target = safe_join(&root, &req.path)?;
    ensure_inside(&root, &target)?;
    if !target.exists() {
        return Err(anyhow::anyhow!("archive not found: {}", req.path).into());
    }
    let dest_dir = target.parent().unwrap_or(&root);
    let file = std::fs::File::open(&target).map_err(|e| anyhow::anyhow!("open: {e}"))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(dest_dir).map_err(|e| anyhow::anyhow!("extract: {e}"))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Uniform API error type so handlers can use `?`.
#[derive(Debug)]
pub struct ApiError(pub anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            format!("{{\"error\":{}}}", serde_json::json!(self.0.to_string())),
        )
            .into_response()
    }
}
