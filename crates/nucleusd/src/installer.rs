use crate::state::{AppState, InstallJob};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use nucleus_core::{detect_pack_kind, CurseForgeManifest, ModrinthIndex, PackKind};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const CONCURRENCY: usize = 8;

/// Result of inspecting a pack before creating a server: lets the panel pick
/// the right docker image and startup command automatically.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PackInsight {
    pub kind: String,
    pub name: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: Option<String>,
    #[serde(rename = "loaderVersion")]
    pub loader_version: Option<String>,
    #[serde(rename = "recommendedImage")]
    pub recommended_image: Option<String>,
    #[serde(rename = "recommendedStartup")]
    pub recommended_startup: Option<String>,
}

fn java_major_for_mc(mc: &str) -> u32 {
    // MC 1.20.5+ needs Java 21, 1.18-1.20.4 needs 17+, older 1.17 needs 16.
    let bad = |maj: u32| maj;
    let _ = bad;
    let parts: Vec<u32> = mc
        .split('.')
        .filter_map(|p| p.split('-').next().and_then(|x| x.parse().ok()))
        .collect();
    if parts.len() >= 2 {
        if parts[0] > 1 || (parts[0] == 1 && parts[1] >= 21) || (parts[0] == 1 && parts[1] == 20 && parts.get(2).copied().unwrap_or(0) >= 5) {
            return 21;
        }
        if parts[0] == 1 && parts[1] >= 18 {
            return 17;
        }
        if parts[0] == 1 && parts[1] == 17 {
            return 17;
        }
    }
    8
}

pub fn inspect_pack(data: &[u8]) -> Result<PackInsight> {
    if data.len() < 4 || &data[..2] != b"PK" {
        bail!("not a zip archive");
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data.to_vec()))
        .map_err(|e| anyhow!("invalid zip: {e}"))?;
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    let kind = detect_pack_kind(&names).ok_or_else(|| anyhow!("empty archive"))?;

    let mut ins = PackInsight {
        kind: format!("{kind:?}").to_lowercase(),
        name: String::new(),
        mc_version: String::new(),
        loader: None,
        loader_version: None,
        recommended_image: None,
        recommended_startup: None,
    };

    match kind {
        PackKind::CurseForge => {
            let manifest =
                CurseForgeManifest::parse(&read_entry_by_base(&mut archive, "manifest.json")
                    .ok_or_else(|| anyhow!("manifest.json missing"))?)?;
            ins.name = manifest.name.clone();
            ins.mc_version = manifest.minecraft.version.clone();
            if let Some((loader, lver)) = manifest.primary_loader() {
                let major = java_major_for_mc(&ins.mc_version);
                ins.loader = Some(loader.clone());
                ins.loader_version = Some(lver.clone());
                ins.recommended_image =
                    Some(format!("ghcr.io/pterodactyl/yolks:java_{major}"));
                ins.recommended_startup = Some(match loader.as_str() {
                    "forge" => "bash run.sh".to_string(),
                    "fabric" => {
                        "java -Xms128M -Xmx{{SERVER_MEMORY}}M -jar fabric-server-launch.jar nogui"
                            .to_string()
                    }
                    _ => "java -jar server.jar".to_string(),
                });
            }
        }
        PackKind::Modrinth => {
            let idx = ModrinthIndex::parse(
                &read_entry_by_base(&mut archive, "modrinth.index.json")
                    .ok_or_else(|| anyhow!("modrinth.index.json missing"))?,
            )?;
            ins.name = idx.name.clone();
            ins.mc_version = idx.game_version.clone();
            let major = java_major_for_mc(&ins.mc_version);
            ins.recommended_image = Some(format!("ghcr.io/pterodactyl/yolks:java_{major}"));
            ins.loader = Some("unknown".into());
        }
        PackKind::ServerPack => {
            ins.name = "server pack".into();
        }
    }
    Ok(ins)
}

pub fn start_pack_install(
    state: Arc<AppState>,
    id: &str,
    filename: String,
    data: Bytes,
) -> Result<()> {
    let rt = state.get(id)?;
    if let Some(existing) = state.installs.get(id) {
        if *existing.state.lock().unwrap() == "running" {
            bail!("install already running");
        }
    }
    let job = Arc::new(InstallJob::default());
    job.set_state("running");
    state.installs.insert(id.to_string(), job.clone());

    let sid = id.to_string();
    tokio::spawn(async move {
        let res = run_install(&state, &rt, &sid, &filename, data, &job).await;
        match res {
            Ok(summary) => {
                job.log(format!("[installer] done: {summary}"));
                job.set_state("done");
            }
            Err(e) => {
                job.log(format!("[installer] FAILED: {e:#}"));
                job.set_state("failed");
            }
        }
    });
    Ok(())
}

pub fn install_status(state: &AppState, id: &str) -> nucleus_core::InstallStatus {
    match state.installs.get(id) {
        Some(job) => nucleus_core::InstallStatus {
            state: job.state.lock().unwrap().clone(),
            lines: job.lines.lock().unwrap().clone(),
        },
        None => nucleus_core::InstallStatus {
            state: "idle".into(),
            lines: vec![],
        },
    }
}

fn log_line(job: &InstallJob, rt: &crate::state::ServerRuntime, msg: &str) {
    job.log(format!("[installer] {msg}"));
    rt.push_log(&format!("[installer] {msg}"));
}

async fn run_install(
    state: &Arc<AppState>,
    rt: &Arc<crate::state::ServerRuntime>,
    _id: &str,
    filename: &str,
    data: Bytes,
    job: &Arc<InstallJob>,
) -> Result<String> {
    let server_dir = rt.server_dir(&state.cfg);
    tokio::fs::create_dir_all(&server_dir)
        .await
        .context("creating server directory")?;

    log_line(
        job,
        rt,
        &format!("received pack {filename} ({} bytes)", data.len()),
    );

    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| anyhow!("not a valid zip: {e}"))?;

    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    let kind = detect_pack_kind(&names).ok_or_else(|| anyhow!("empty archive"))?;

    let mut meta = InstallMeta {
        kind: format!("{kind:?}").to_lowercase(),
        ..Default::default()
    };

    match kind {
        PackKind::CurseForge => {
            log_line(job, rt, "detected CurseForge modpack");
            let manifest_json = read_entry_by_base(&mut archive, "manifest.json")
                .ok_or_else(|| anyhow!("manifest.json missing"))?;
            let manifest = CurseForgeManifest::parse(&manifest_json)?;
            meta.name = manifest.name.clone();
            meta.version = manifest.version.clone();
            meta.mc_version = manifest.minecraft.version.clone();
            if let Some((loader, lver)) = manifest.primary_loader() {
                log_line(
                    job,
                    rt,
                    &format!(
                        "requires Minecraft {mc} with {loader} {lver}",
                        mc = meta.mc_version,
                    ),
                );
                meta.loader = Some(loader.clone());
                meta.loader_version = Some(lver.clone());
            }
            let copied = extract_prefix(&mut archive, &manifest.overrides, &server_dir, job, rt)?;
            log_line(job, rt, &format!("extracted {copied} override files"));

            let key = state.cfg.cf_api_key();
            if key.is_none() {
                log_line(
                    job,
                    rt,
                    "no curseforge_api_key configured; using public download endpoint",
                );
            }
            let mods_dir = server_dir.join("mods");
            tokio::fs::create_dir_all(&mods_dir).await?;

            let total = manifest.files.len();
            let ok = AtomicUsize::new(0);
            let fail = AtomicUsize::new(0);
            let done_ct = AtomicUsize::new(0);

            let futures = manifest.files.iter().map(|f| {
                let st = state.clone();
                let mods_dir = mods_dir.clone();
                let api_key = key.clone();
                let ok = &ok;
                let fail = &fail;
                let done_ct = &done_ct;
                let job = job.clone();
                let rt2 = rt.clone();
                async move {
                    match fetch_cf_file(
                        &st.http,
                        api_key.as_deref(),
                        f.project_id,
                        f.file_id,
                        &mods_dir,
                    )
                    .await
                    {
                        Ok(fname) => {
                            ok.fetch_add(1, Ordering::Relaxed);
                            log_line(&job, &rt2, &format!("mod installed: {fname}"));
                        }
                        Err(e) => {
                            fail.fetch_add(1, Ordering::Relaxed);
                            log_line(
                                &job,
                                &rt2,
                                &format!(
                                    "mod FAILED (project {} file {}): {e:#}",
                                    f.project_id, f.file_id
                                ),
                            );
                        }
                    }
                    let d = done_ct.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 25 == 0 || d == total {
                        log_line(&job, &rt2, &format!("progress {d}/{total}"));
                    }
                }
            });

            StreamExt::for_each_concurrent(
                futures_util::stream::iter(futures),
                CONCURRENCY,
                |fut| fut,
            )
            .await;

            let o = ok.load(Ordering::Relaxed);
            let f = fail.load(Ordering::Relaxed);
            meta.mods_installed = Some(o);
            meta.mods_failed = Some(f);
            if f > 0 && o == 0 {
                bail!("all {f} mod downloads failed");
            }
            log_line(job, rt, &format!("mods installed={o} failed={f}"));
            if let (Some(loader), Some(lver)) =
                (meta.loader.clone(), meta.loader_version.clone())
            {
                setup_loader(state, rt, job, &loader, &meta.mc_version, &lver).await;
            }
        }
        PackKind::Modrinth => {
            log_line(job, rt, "detected Modrinth modpack");
            let idx_json = read_entry_by_base(&mut archive, "modrinth.index.json")
                .ok_or_else(|| anyhow!("modrinth.index.json missing"))?;
            let index = ModrinthIndex::parse(&idx_json)?;
            meta.name = index.name.clone();
            meta.version = index.version_id.clone();
            meta.mc_version = index.game_version.clone();

            let mut copied = extract_prefix(&mut archive, "overrides", &server_dir, job, rt)?;
            copied += extract_prefix(&mut archive, "server-overrides", &server_dir, job, rt)?;
            log_line(job, rt, &format!("extracted {copied} override files"));

            let mut ok = 0usize;
            let mut fail = 0usize;
            for mf in &index.files {
                let url = match mf.downloads.first() {
                    Some(u) => u.clone(),
                    None => continue,
                };
                let dest = safe_rel_dest(&server_dir, &mf.path)?;
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await.ok();
                }
                match download(&state.http, &url, None, &dest).await {
                    Ok(_) => {
                        ok += 1;
                        log_line(job, rt, &format!("file installed: {}", mf.path));
                    }
                    Err(e) => {
                        fail += 1;
                        log_line(job, rt, &format!("file FAILED {}: {e:#}", mf.path));
                    }
                }
            }
            meta.mods_installed = Some(ok);
            meta.mods_failed = Some(fail);
            log_line(job, rt, &format!("files installed={ok} failed={fail}"));
        }
        PackKind::ServerPack => {
            log_line(job, rt, "detected generic server pack");
            let copied = extract_flat(&mut archive, &server_dir, job, rt)?;
            log_line(job, rt, &format!("extracted {copied} files"));
            meta.name = filename.to_string();
        }
    }

    if rt.spec.accept_eula {
        tokio::fs::write(server_dir.join("eula.txt"), "eula=true\n")
            .await
            .ok();
        log_line(job, rt, "eula.txt accepted");
    }

    meta.installed_at = chrono::Utc::now().to_rfc3339();
    let meta_path = server_dir.join(".nucleus-install.json");
    tokio::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)
        .await
        .context("writing install metadata")?;

    Ok(format!(
        "{} v{} (mc {}) complete",
        meta.name, meta.version, meta.mc_version
    ))
}

#[derive(Serialize, Default)]
struct InstallMeta {
    kind: String,
    name: String,
    #[serde(rename = "version")]
    version: String,
    #[serde(rename = "mcVersion")]
    mc_version: String,
    loader: Option<String>,
    #[serde(rename = "loaderVersion")]
    loader_version: Option<String>,
    #[serde(rename = "modsInstalled")]
    mods_installed: Option<usize>,
    #[serde(rename = "modsFailed")]
    mods_failed: Option<usize>,
    #[serde(rename = "installedAt")]
    installed_at: String,
}

async fn fetch_cf_file(
    http: &reqwest::Client,
    api_key: Option<&str>,
    project_id: u64,
    file_id: u64,
    dest_dir: &Path,
) -> Result<String> {
    let mut file_name: Option<String> = None;
    let mut download_url: Option<String> = None;

    if let Some(key) = api_key {
        let url = format!("https://api.curseforge.com/v1/mods/{project_id}/files/{file_id}");
        let resp = http
            .get(&url)
            .header("x-api-key", key)
            .header("accept", "application/json")
            .send()
            .await
            .context("curseforge api request")?;
        if resp.status().is_success() {
            let v: serde_json::Value = resp.json().await.context("cf api json")?;
            file_name = v["data"]["fileName"].as_str().map(str::to_owned);
            download_url = v["data"]["downloadUrl"].as_str().map(str::to_owned);
        }
    }

    let url = download_url.unwrap_or_else(|| {
        format!("https://www.curseforge.com/api/v1/mods/{project_id}/files/{file_id}/download")
    });

    // Download first so we can derive the real file name from the final redirect URL.
    let mut req = http.get(&url);
    if url.contains("curseforge.com") || url.contains("forgecdn.net") {
        if let Some(k) = api_key {
            req = req.header("x-api-key", k);
        }
    }
    let resp = req.send().await.context("download request")?;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status());
    }
    let final_url = resp.url().to_string();
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body from {url}"))?;
    if bytes.is_empty() {
        bail!("empty response");
    }

    let fname = match file_name {
        Some(n) => sanitize_name(&n).ok_or_else(|| anyhow!("invalid file name"))?,
        None => {
            let candidate = final_url
                .split('?')
                .next()
                .and_then(|u| u.rsplit('/').next())
                .unwrap_or("");
            let derived = sanitize_name(candidate).unwrap_or_else(|| "mod.jar".into());
            if derived == "download" || !derived.contains('.') {
                format!("{file_id}.jar")
            } else {
                derived
            }
        }
    };

    if &bytes[..2.min(bytes.len())] != b"PK" {
        bail!("downloaded {fname} is not a jar/zip archive");
    }

    let dest = safe_rel_dest(dest_dir, &fname)?;
    tokio::fs::write(&dest, &bytes)
        .await
        .context("writing file")?;
    Ok(fname)
}

async fn download(
    http: &reqwest::Client,
    url: &str,
    cf_key: Option<&str>,
    dest: &Path,
) -> Result<()> {
    let mut req = http.get(url);
    if url.contains("curseforge.com") || url.contains("forgecdn.net") {
        if let Some(k) = cf_key {
            req = req.header("x-api-key", k);
        }
    }
    let resp = req.send().await.context("download request")?;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status());
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body from {url}"))?;
    if bytes.is_empty() {
        bail!("empty response");
    }
    tokio::fs::write(dest, &bytes).await.context("writing file")
}

fn sanitize_name(name: &str) -> Option<String> {
    let base = name.replace('\\', "/").rsplit('/').next()?.to_string();
    if base.is_empty() || base == "." || base == ".." {
        None
    } else {
        Some(base)
    }
}

fn safe_rel_dest(base: &Path, rel: &str) -> Result<PathBuf> {
    let cleaned = sanitize_name(rel).ok_or_else(|| anyhow!("invalid file name"))?;
    Ok(base.join(cleaned))
}

fn read_entry_by_base<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    base: &str,
) -> Option<String> {
    let mut idx = None;
    for i in 0..archive.len() {
        if let Ok(f) = archive.by_index(i) {
            let n = f.name().rsplit('/').next().unwrap_or("");
            if n == base {
                idx = Some(i);
                break;
            }
        }
    }
    let i = idx?;
    let mut f = archive.by_index(i).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Extract every entry whose path begins with `<prefix>/` into `dest`, preserving subpaths.
/// Returns number of files written.
fn extract_prefix<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    prefix: &str,
    dest: &Path,
    job: &Arc<InstallJob>,
    rt: &Arc<crate::state::ServerRuntime>,
) -> Result<usize> {
    let pfx = format!("{prefix}/");
    let mut written = 0usize;
    for i in 0..archive.len() {
        let mut f = match archive.by_index(i) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let name = f.name().to_string();
        if !name.starts_with(&pfx) {
            continue;
        }
        let rel = &name[pfx.len()..];
        if rel.is_empty() {
            continue;
        }
        if f.is_dir() {
            let _ = std::fs::create_dir_all(dest.join(rel));
            continue;
        }
        let out_path = dest.join(rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        std::io::copy(&mut f, &mut out).context("extracting file")?;
        written += 1;
        if written % 200 == 0 {
            log_line(job, rt, &format!("extracted {written} files…"));
        }
    }
    Ok(written)
}

/// Extract everything, stripping one common top-level folder when present.
fn extract_flat<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    dest: &Path,
    job: &Arc<InstallJob>,
    rt: &Arc<crate::state::ServerRuntime>,
) -> Result<usize> {
    let mut roots: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..archive.len() {
        if let Ok(f) = archive.by_index(i) {
            if let Some(p) = f.enclosed_name() {
                if let Some(top) = p.components().next() {
                    *roots
                        .entry(top.as_os_str().to_string_lossy().to_string())
                        .or_default() += 1;
                }
            }
        }
    }
    let strip: Option<String> = if roots.len() == 1 {
        roots.keys().next().cloned()
    } else {
        None
    };
    if let Some(root) = &strip {
        log_line(job, rt, &format!("stripping archive root '{root}/'"));
    }

    let mut written = 0usize;
    for i in 0..archive.len() {
        let mut f = match archive.by_index(i) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let rel = match f.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let rel: PathBuf = match &strip {
            Some(root) => {
                let comps: PathBuf = rel.components().skip(1).collect();
                if comps.as_os_str().is_empty() {
                    continue;
                }
                let _ = root;
                comps
            }
            None => rel,
        };
        if f.is_dir() {
            let _ = std::fs::create_dir_all(dest.join(&rel));
            continue;
        }
        let out_path = dest.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        std::io::copy(&mut f, &mut out).context("extracting file")?;
        written += 1;
        if written % 200 == 0 {
            log_line(job, rt, &format!("extracted {written} files…"));
        }
    }
    Ok(written)
}

/// Install the mod loader into the server directory so the pack is runnable:
///  - forge: run the official installer in a one-off container (generates run.sh)
///  - fabric: fetch the fabric-server-launch jar from the Fabric meta
async fn setup_loader(
    state: &Arc<AppState>,
    rt: &Arc<crate::state::ServerRuntime>,
    job: &Arc<InstallJob>,
    loader: &str,
    mc: &str,
    lver: &str,
) {
    let server_dir = rt.server_dir(&state.cfg);
    let result = match loader {
        "forge" => {
            let candidates = [
                format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{mc}-{lver}/forge-{mc}-{lver}-installer.jar"),
                format!("https://bmclapi2.bangbang93.com/maven/net/minecraftforge/forge/{mc}-{lver}/forge-{mc}-{lver}-installer.jar"),
                format!("https://files.minecraftforge.net/net/minecraftforge/forge/{mc}-{lver}/forge-{mc}-{lver}-installer.jar"),
            ];
            log_line(job, rt, &format!("downloading forge installer {mc}-{lver}"));
            let mut got = Err(anyhow!("no source attempted"));
            for (i, url) in candidates.iter().enumerate() {
                match fetch_ipv4(url, &server_dir.join("forge-installer.jar")).await {
                    Ok(()) => {
                        got = Ok(());
                        break;
                    }
                    Err(e) => {
                        log_line(job, rt, &format!("source {} failed ({e:#}); trying next…", i + 1));
                        got = Err(e);
                    }
                }
            }
            if let Err(e) = got {
                Err(anyhow!("fetching forge installer: {e:#}"))
            } else {
                run_oneoff(
                    state,
                    job,
                    rt,
                    "forge-installer",
                    format!(
                        "cd /data && java -jar forge-installer.jar --installServer . ; rc=$?; rm -f forge-installer.jar; exit $rc"
                    ),
                    None,
                )
                .await
                .map(|_| log_line(job, rt, "forge installed (run.sh generated)"))
            }
        }
        "fabric" => {
            let url = format!(
                "https://meta.fabricmc.net/v2/versions/loader/{mc}/{lver}/1.0.3/server/jar"
            );
            log_line(job, rt, &format!("fetching fabric-server-launch ({mc}, loader {lver})"));
            match fetch_ipv4(&url, &server_dir.join("fabric-server-launch.jar")).await {
                Ok(()) => {
                    log_line(job, rt, "fabric launcher ready");
                    Ok(())
                }
                Err(e) => Err(anyhow!("fetching fabric launcher: {e:#}")),
            }
        }
        other => {
            log_line(job, rt, &format!("loader '{other}' is not auto-installed yet; install it manually"));
            return;
        }
    };
    if let Err(e) = result {
        log_line(job, rt, &format!("loader setup FAILED: {e:#}"));
    }
}

/// Run a short-lived container sharing the server's data dir (used for the forge installer).
async fn run_oneoff(
    state: &Arc<AppState>,
    job: &Arc<InstallJob>,
    rt: &Arc<crate::state::ServerRuntime>,
    tag: &str,
    cmd: String,
    image: Option<String>,
) -> Result<()> {
    use futures_util::StreamExt;
    // Some egg JSONs embed CRLF line endings which bash chokes on ($'\r').
    let cmd = cmd.replace("\r\n", "\n");
    let name = format!("nucleus-{}-{tag}", rt.spec.id);
    let dir = rt.server_dir(&state.cfg).canonicalize().unwrap_or_else(|_| rt.server_dir(&state.cfg));
    // Pterodactyl egg scripts expect their server files at well-known
    // container paths (/mnt/server for newer eggs, /home/container for older
    // ones). Bind the server dir under all of its aliases so stock scripts
    // work unmodified.
    let d = dir.display();
    let host_config = bollard::secret::HostConfig {
        binds: Some(vec![
            format!("{d}:/data"),
            format!("{d}:/home/container"),
            format!("{d}:/mnt/server"),
        ]),
        ..Default::default()
    };
    let env: Vec<String> = crate::docker::container_env(&rt.spec);
    let cfg = bollard::container::Config {
        image: Some(image.unwrap_or_else(|| rt.spec.image.clone())),
        entrypoint: Some(vec!["/bin/bash".into(), "-c".into()]),
        cmd: Some(vec![cmd]),
        env: Some(env),
        working_dir: Some("/data".into()),
        user: Some("0:0".into()),
        host_config: Some(host_config),
        labels: Some(
            [(
                "nucleus.oneoff",
                format!("{}:{tag}", rt.spec.id),
            )]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ),
        ..Default::default()
    };
    let _ = state.docker.remove_container(&name, Some(bollard::container::RemoveContainerOptions { force: true, ..Default::default() })).await;
    state
        .docker
        .create_container(
            Some(bollard::container::CreateContainerOptions { name: name.clone(), platform: None }),
            cfg,
        )
        .await?;
    state.docker.start_container(&name, None::<bollard::container::StartContainerOptions<String>>).await?;

    // Stream output lines into the install job while it runs.
    let logs_opts = bollard::container::LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        since: 0,
        until: 0,
        timestamps: false,
        tail: "0".to_string(),
    };
    let mut stream = state.docker.logs(&name, Some(logs_opts));
    while let Some(chunk) = stream.next().await {
        if let Ok(out) = chunk {
            let text = match out {
                bollard::container::LogOutput::StdErr { message }
                | bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::Console { message } => {
                    String::from_utf8_lossy(&message).to_string()
                }
                _ => continue,
            };
            for line in text.lines().filter(|l| !l.trim().is_empty()).take(400) {
                log_line(job, rt, line);
            }
        }
    }

    let status = state.docker.inspect_container(&name, None).await?;
    let code = status.state.as_ref().and_then(|s| s.exit_code).unwrap_or(-1);
    let _ = state.docker.remove_container(&name, Some(bollard::container::RemoveContainerOptions { force: true, ..Default::default() })).await;
    if code != 0 {
        bail!("{tag} exited with code {code}");
    }
    Ok(())
}

/// Some modding CDNs (notably maven.minecraftforge.net) serve 404s over
/// broken IPv6 routes; force IPv4 and retry a few times.
async fn fetch_ipv4(url: &str, dest: &Path) -> Result<()> {
    let mut last = String::new();
    for attempt in 0..4 {
        let client = reqwest::Client::builder()
            .user_agent("nucleusd/0.1")
            .timeout(std::time::Duration::from_secs(600))
            .local_address(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)))
            .build()?;
        match client.get(url).header("accept", "*/*").send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) if bytes.len() > 1024 && &bytes[..2] == b"PK" => {
                    tokio::fs::write(dest, &bytes).await?;
                    return Ok(());
                }
                Ok(_) => last = "response was not a jar".into(),
                Err(e) => last = format!("body read: {e}"),
            },
            Ok(resp) => {
                last = format!("HTTP {}", resp.status());
                let hdrs = format!("{:?}", resp.headers());
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(attempt, status=%last, url=%url, "loader download failed");
                tracing::warn!(%hdrs, snippet=%truncate_body(&body), "loader download response");
            }
            Err(e) => {
                last = e.to_string();
                tracing::warn!(attempt, error=%e, "loader download");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2 * (attempt as u64 + 1))).await;
    }
    anyhow::bail!("{last}")
}

fn truncate_body(s: &str) -> String {
    s.chars().take(300).collect()
}


// ── egg install-script execution ─────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredScript {
    pub script: String,
    pub image: Option<String>,
}

fn scripts_path(cfg: &crate::config::Config) -> PathBuf {
    cfg.data_dir.join("scripts.json")
}

pub fn store_script(cfg: &crate::config::Config, id: &str, s: StoredScript) {
    let mut all: std::collections::BTreeMap<String, StoredScript> =
        std::fs::read_to_string(scripts_path(cfg))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
    all.insert(id.to_string(), s);
    if let Ok(json) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::create_dir_all(&cfg.data_dir);
        let _ = std::fs::write(scripts_path(cfg), json);
    }
}

pub fn load_script(cfg: &crate::config::Config, id: &str) -> Option<StoredScript> {
    let all: std::collections::BTreeMap<String, StoredScript> =
        std::fs::read_to_string(scripts_path(cfg))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())?;
    all.get(id).cloned()
}

/// Run a stored/attached egg install script as a sidecar job, streaming output
/// into the per-server install log (Modpacks tab / console).
pub fn start_script_install(
    state: Arc<AppState>,
    rt: Arc<crate::state::ServerRuntime>,
    script: String,
    image: Option<String>,
) -> Result<()> {
    if let Some(existing) = state.installs.get(&rt.spec.id) {
        if *existing.state.lock().unwrap() == "running" {
            anyhow::bail!("another install is running");
        }
    }
    store_script(
        &state.cfg,
        &rt.spec.id,
        StoredScript { script: script.clone(), image: image.clone() },
    );

    let job = Arc::new(InstallJob::default());
    job.set_state("running");
    state
        .installs
        .insert(rt.spec.id.clone(), job.clone());

    tokio::spawn(async move {
        // Mirror installer lifecycle into the server console so users (and
        // remote debugging) see progress without opening the install status.
        job.log("[installer] running egg install script…");
        rt.push_log("[installer] running egg install script…");
        let img_owned = image.clone().unwrap_or_else(|| rt.spec.image.clone());
        if !crate::docker::image_exists(&state.docker, &img_owned).await {
            job.log(&format!("[installer] pulling image {img_owned}…"));
            rt.push_log(&format!("[installer] pulling image {img_owned}…"));
            match crate::docker::pull_image(&state.docker, &img_owned).await {
                Err(e) => {
                    let msg = format!("[installer] image pull FAILED: {e:#}");
                    job.log(&msg);
                    rt.push_log(&msg);
                    job.set_state("failed");
                    return;
                }
                Ok(()) => {
                    // Belt and braces: verify the image actually landed
                    // before container create 404s with a cryptic message.
                    if !crate::docker::image_exists(&state.docker, &img_owned).await {
                        let msg = format!(
                            "[installer] image pull reported success but {img_owned} is still missing (disk full?)"
                        );
                        job.log(&msg);
                        rt.push_log(&msg);
                        job.set_state("failed");
                        return;
                    }
                    let msg = format!("[installer] image {img_owned} ready");
                    job.log(&msg);
                    rt.push_log(&msg);
                }
            }
        }
        let cmd = format!(
            "cd /data && set -e\n{}",
            script
        );
        let res = run_oneoff(&state, &job, &rt, "install", cmd, image).await;
        match res {
            Ok(()) => {
                job.log("[installer] install script finished successfully");
                job.set_state("done");
                rt.push_log("[installer] egg install completed");
            }
            Err(e) => {
                job.log(format!("[installer] FAILED: {e:#}"));
                job.set_state("failed");
                rt.push_log(&format!("[installer] egg install failed: {e:#}"));
            }
        }
    });
    Ok(())
}
