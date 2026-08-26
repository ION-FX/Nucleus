use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;

/// Embedded SSH/SFTP server. Users authenticate as `srv.<server-id>` with a
/// per-server password and are jailed to that server's data directory.
pub async fn run(app: Arc<AppState>) -> Result<()> {
    let key_path = app.cfg.data_dir.join("sftp_hostkey");
    let host_key = match std::fs::read_to_string(&key_path) {
        Ok(pem) => russh::keys::PrivateKey::from_openssh(&pem)
            .context("parsing sftp host key")?,
        Err(_) => {
            let mut rng = rand_010::rng();
            let key = russh::keys::PrivateKey::random(
                &mut rng,
                russh::keys::Algorithm::Ed25519,
            )
            .map_err(|e| anyhow!("generating host key: {e}"))?;
            std::fs::create_dir_all(&app.cfg.data_dir).ok();
            let pem = key
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .map_err(|e| anyhow!("encoding host key: {e}"))?;
            std::fs::write(&key_path, pem.as_bytes())?;
            key
        }
    };

    let config = Arc::new(russh::server::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        auth_rejection_time: std::time::Duration::from_secs(1),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        keys: vec![host_key],
        ..Default::default()
    });

    let bind = app.cfg.sftp.bind.clone();
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "sftp server listening");

    let mut server = SshServer { app };
    server
        .run_on_socket(config, &listener)
        .await
        .map_err(|e| anyhow!("sftp server error: {e}"))
}

struct SshServer {
    app: Arc<AppState>,
}

impl russh::server::Server for SshServer {
    type Handler = Conn;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Conn {
        Conn { app: self.app.clone(), session_server: None, root: None, channel: None }
    }
}

struct Conn {
    app: Arc<AppState>,
    session_server: Option<String>,
    root: Option<PathBuf>,
    channel: Option<Channel<Msg>>,
}

impl russh::server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let id = user.strip_prefix("srv.").unwrap_or(user);
        if !self.app.servers.contains_key(id) {
            return Ok(Auth::reject());
        }
        if constant_time_eq(password.as_bytes(), self.app.sftp_password(id).as_bytes()) {
            self.session_server = Some(id.to_string());
            self.root = Some(self.app.cfg.servers_dir().join(id));
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        self.channel = Some(channel);
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" || self.session_server.is_none() {
            let _ = session.channel_failure(channel);
            return Ok(());
        }
        let _ = session.channel_success(channel);
        if let Some(ch) = self.channel.take() {
            let root = self.root.clone().unwrap_or_default();
            let stream = ch.into_stream();
            tokio::spawn(async move {
                russh_sftp::server::run(stream, FsHandler::new(root)).await;
            });
        }
        Ok(())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── filesystem-backed SFTP handler ───────────────────────────────────────

enum HandleKind {
    File(std::fs::File),
    Dir(std::fs::ReadDir),
}

struct FsHandler {
    root: PathBuf,
    handles: HashMap<String, HandleKind>,
}

impl FsHandler {
    fn new(root: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&root);
        Self { root, handles: HashMap::new() }
    }

    /// Join a client path onto the jail root, rejecting traversal.
    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        use std::path::Component;
        let rel = path.trim_start_matches('/');
        let mut depth: i32 = 0;
        for c in Path::new(rel).components() {
            match c {
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(StatusCode::PermissionDenied);
                    }
                }
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                _ => return Err(StatusCode::BadMessage),
            }
        }
        let joined = self.root.join(rel);
        // containment check through any existing ancestor (symlink safety)
        let root_canon =
            self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let mut probe = joined.clone();
        while !probe.exists() {
            match probe.parent() {
                Some(p) => probe = p.to_path_buf(),
                None => break,
            }
        }
        if probe.exists() {
            let canon = probe.canonicalize().map_err(|e| status_from_io(&e))?;
            if !canon.starts_with(&root_canon) {
                return Err(StatusCode::PermissionDenied);
            }
        }
        Ok(joined)
    }
}

fn status_from_io(e: &std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

fn attrs_from(md: &std::fs::Metadata) -> russh_sftp::protocol::FileAttributes {
    md.into()
}

fn file_entry(_path: &Path, name: String, md: &std::fs::Metadata) -> russh_sftp::protocol::File {
    russh_sftp::protocol::File::new(name, attrs_from(md))
}

fn new_handle() -> String {
    let mut buf = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl russh_sftp::server::Handler for FsHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<russh_sftp::protocol::Version, StatusCode> {
        Ok(russh_sftp::protocol::Version::new())
    }

    async fn realpath(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Name, StatusCode> {
        use std::path::Component;
        let rel = path.trim_start_matches('/');
        let mut parts: Vec<String> = Vec::new();
        for c in Path::new(rel).components() {
            match c {
                Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
                Component::ParentDir => {
                    parts.pop();
                }
                _ => {}
            }
        }
        let mut out = "/".to_string();
        for p in &parts {
            out.push_str(p);
            out.push('/');
        }
        let full = out.trim_end_matches('/').to_string();
        Ok(russh_sftp::protocol::Name {
            id,
            files: vec![russh_sftp::protocol::File::dummy(full)],
        })
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: russh_sftp::protocol::OpenFlags,
        _attrs: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Handle, StatusCode> {
        let target = self.resolve(&filename)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(pflags.contains(russh_sftp::protocol::OpenFlags::READ));
        opts.write(pflags.contains(russh_sftp::protocol::OpenFlags::WRITE));
        opts.append(pflags.contains(russh_sftp::protocol::OpenFlags::APPEND));
        opts.create(pflags.contains(russh_sftp::protocol::OpenFlags::CREATE));
        opts.truncate(pflags.contains(russh_sftp::protocol::OpenFlags::TRUNCATE));
        if !pflags.contains(russh_sftp::protocol::OpenFlags::CREATE)
            && pflags.contains(russh_sftp::protocol::OpenFlags::EXCLUDE)
        {
            opts.create_new(true);
        }
        let f = opts.open(&target).map_err(|e| status_from_io(&e))?;
        let handle = new_handle();
        self.handles.insert(handle.clone(), HandleKind::File(f));
        Ok(russh_sftp::protocol::Handle { id, handle })
    }

    async fn close(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        self.handles.remove(&handle);
        Ok(russh_sftp::protocol::Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en".into(),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<russh_sftp::protocol::Data, StatusCode> {
        use std::os::unix::fs::FileExt;
        let Some(HandleKind::File(f)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let mut buf = vec![0u8; len as usize];
        let n = f.read_at(&mut buf, offset).map_err(|e| status_from_io(&e))?;
        buf.truncate(n);
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        Ok(russh_sftp::protocol::Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        use std::os::unix::fs::FileExt;
        let Some(HandleKind::File(f)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        f.write_all_at(&data, offset).map_err(|e| status_from_io(&e))?;
        Ok(russh_sftp::protocol::Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en".into(),
        })
    }

    async fn stat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, StatusCode> {
        let target = self.resolve(&path)?;
        let md = target.metadata().map_err(|e| status_from_io(&e))?;
        Ok(russh_sftp::protocol::Attrs { id, attrs: attrs_from(&md) })
    }

    async fn lstat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, StatusCode> {
        let target = self.resolve(&path)?;
        let md = target.symlink_metadata().map_err(|e| status_from_io(&e))?;
        Ok(russh_sftp::protocol::Attrs { id, attrs: attrs_from(&md) })
    }

    async fn fstat(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Attrs, StatusCode> {
        let Some(HandleKind::File(f)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let md = f.metadata().map_err(|e| status_from_io(&e))?;
        Ok(russh_sftp::protocol::Attrs { id, attrs: attrs_from(&md) })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        apply_attrs(&self.resolve(&path)?, &attrs)?;
        Ok(ok_status(id))
    }

    async fn opendir(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Handle, StatusCode> {
        let target = self.resolve(&path)?;
        let rd = std::fs::read_dir(&target).map_err(|e| status_from_io(&e))?;
        let handle = new_handle();
        self.handles.insert(handle.clone(), HandleKind::Dir(rd));
        Ok(russh_sftp::protocol::Handle { id, handle })
    }

    async fn readdir(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Name, StatusCode> {
        let Some(HandleKind::Dir(rd)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        let mut files = Vec::new();
        for entry in rd.by_ref().take(100) {
            let Ok(entry) = entry else { continue };
            let Ok(md) = entry.metadata() else { continue };
            files.push(file_entry(
                &entry.path(),
                entry.file_name().to_string_lossy().to_string(),
                &md,
            ));
        }
        if files.is_empty() {
            self.handles.remove(&handle);
            return Err(StatusCode::Eof);
        }
        Ok(russh_sftp::protocol::Name { id, files })
    }

    async fn remove(
        &mut self,
        id: u32,
        filename: String,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        let target = self.resolve(&filename)?;
        if target.is_dir() {
            return Err(StatusCode::Failure);
        }
        std::fs::remove_file(target).map_err(|e| status_from_io(&e))?;
        Ok(ok_status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        let target = self.resolve(&path)?;
        std::fs::create_dir(target).map_err(|e| status_from_io(&e))?;
        Ok(ok_status(id))
    }

    async fn rmdir(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        let target = self.resolve(&path)?;
        std::fs::remove_dir(target).map_err(|e| status_from_io(&e))?;
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        let from = self.resolve(&oldpath)?;
        let to = self.resolve(&newpath)?;
        if from.is_dir() {
            std::fs::rename(from, to).map_err(|e| status_from_io(&e))?;
            return Ok(ok_status(id));
        }
        std::fs::rename(from, to).map_err(|e| status_from_io(&e))?;
        Ok(ok_status(id))
    }

    async fn readlink(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Name, StatusCode> {
        let target = self.resolve(&path)?;
        let dest = std::fs::read_link(target).map_err(|e| status_from_io(&e))?;
        Ok(russh_sftp::protocol::Name {
            id,
            files: vec![russh_sftp::protocol::File::dummy(dest.to_string_lossy().to_string())],
        })
    }

    async fn symlink(
        &mut self,
        id: u32,
        linkpath: String,
        targetpath: String,
    ) -> Result<russh_sftp::protocol::Status, StatusCode> {
        let link = self.resolve(&linkpath)?;
        std::os::unix::fs::symlink(&targetpath, link).map_err(|e| status_from_io(&e))?;
        Ok(ok_status(id))
    }
}

fn ok_status(id: u32) -> russh_sftp::protocol::Status {
    russh_sftp::protocol::Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en".into(),
    }
}

use russh_sftp::protocol::StatusCode;

fn apply_attrs(
    path: &Path,
    attrs: &russh_sftp::protocol::FileAttributes,
) -> Result<(), StatusCode> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = attrs.permissions {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
            .map_err(|e| status_from_io(&e))?;
    }
    if let (Some(atime), Some(mtime)) = (attrs.atime, attrs.mtime) {
        let t = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(mtime as u64);
        filetime::set_file_times(path, filetime::FileTime::from_system_time(t), filetime::FileTime::from_system_time(t))
            .map_err(|_| StatusCode::Failure)?;
        let _ = atime;
    }
    Ok(())
}
