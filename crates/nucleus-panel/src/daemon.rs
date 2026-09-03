use crate::models::Node;
use anyhow::{Context, Result};
use nucleus_core::{CreateServerRequest, PowerAction, ServerStatus};
use reqwest::Method;

/// Thin REST client for one registered daemon node.
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
    token: String,
    tls_insecure: bool,
    tls_ca_path: Option<String>,
}

impl DaemonClient {
    pub fn new(app: &crate::routes::App, node: &Node) -> Self {
        Self {
            http: app.node_client(node),
            base: node.url.trim_end_matches('/').to_string(),
            token: node.token.clone(),
            tls_insecure: node.tls_insecure,
            tls_ca_path: node.tls_ca_path.clone(),
        }
    }

    fn sign(&self, method: &str, path: &str, ts: i64) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = match Hmac::<sha2::Sha256>::new_from_slice(self.token.as_bytes()) {
            Ok(m) => m,
            Err(_) => return String::new(),
        };
        mac.update(format!("{ts}.{}.{path}", method.to_uppercase()).as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Auth headers for a raw handshake (WebSocket upgrade) to `path`.
    pub fn ws_auth_headers(&self, method: &str, path: &str) -> Vec<(&'static str, String)> {
        let ts = chrono::Utc::now().timestamp();
        vec![
            ("authorization", format!("Bearer {}", self.token)),
            ("x-nucleus-timestamp", ts.to_string()),
            ("x-nucleus-signature", self.sign(method, path, ts)),
        ]
    }

    /// TLS connector for the WebSocket handshake when the node uses custom
    /// trust; None lets tungstenite use its default webpki-roots stack.
    pub fn ws_connector(&self) -> Option<tokio_tungstenite::Connector> {
        if !self.tls_insecure && self.tls_ca_path.is_none() {
            return None;
        }
        let config = if self.tls_insecure {
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyVerifier))
                .with_no_client_auth()
        } else {
            let mut roots = rustls::RootCertStore::empty();
            if let Some(ca) = &self.tls_ca_path {
                let pem = std::fs::read(ca).unwrap_or_default();
                let mut rd = std::io::BufReader::new(pem.as_slice());
                for cert in rustls_pemfile::certs(&mut rd).flatten() {
                    let _ = roots.add(cert);
                }
            }
            if roots.is_empty() {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        };
        Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
            config,
        )))
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let ts = chrono::Utc::now().timestamp();
        self.http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("x-nucleus-timestamp", ts.to_string())
            .header("x-nucleus-signature", self.sign(method.as_str(), path, ts))
            .timeout(std::time::Duration::from_secs(30))
    }

    pub async fn health(&self) -> Result<()> {
        let r = self.req(Method::GET, "/health").send().await?;
        if !r.status().is_success() {
            anyhow::bail!("HTTP {}", r.status());
        }
        Ok(())
    }

    /// Rich node stats for the admin dashboard.
    pub async fn info(&self) -> Result<serde_json::Value> {
        let r = self
            .req(Method::GET, "/api/info")
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        parse(r).await
    }

    /// Ask the daemon to parse a modpack zip and recommend image/startup.
    pub async fn inspect_pack(&self, data: Vec<u8>) -> Result<serde_json::Value> {
        let r = self
            .req(Method::POST, "/api/pack/inspect")
            .timeout(std::time::Duration::from_secs(60))
            .body(data)
            .send()
            .await?;
        parse(r).await
    }

    pub async fn create_server(&self, spec: &CreateServerRequest) -> Result<ServerStatus> {
        // Image pulls (e.g. wine/steam eggs) can take many minutes on first
        // create, so this call gets a much longer timeout than the default 30s.
        let r = self
            .req(Method::POST, "/api/servers")
            .timeout(std::time::Duration::from_secs(1800))
            .json(spec)
            .send()
            .await?;
        parse(r).await
    }

    pub async fn remove_server(&self, id: &str, purge_data: bool) -> Result<()> {
        let r = self
            .req(
                Method::DELETE,
                &format!("/api/servers/{id}?purge_data={purge_data}"),
            )
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn status(&self, id: &str) -> Result<ServerStatus> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn power(&self, id: &str, action: PowerAction) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/power"))
            .json(&nucleus_core::PowerRequest {
                action,
                command: None,
            })
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn logs(&self, id: &str, tail: usize) -> Result<String> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/logs?tail={tail}"))
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {status}");
        }
        Ok(r.text().await?)
    }

    pub async fn list_files(
        &self,
        id: &str,
        path: Option<&str>,
    ) -> Result<Vec<nucleus_core::FileEntry>> {
        let p = path.unwrap_or("/");
        let r = self
            .req(
                Method::GET,
                &format!(
                    "/api/servers/{id}/files/list?path={}",
                    urlencoding::encode(p)
                ),
            )
            .send()
            .await?;
        parse(r).await
    }

    pub async fn read_file(&self, id: &str, path: &str) -> Result<bytes::Bytes> {
        let r = self
            .req(
                Method::GET,
                &format!(
                    "/api/servers/{id}/files/read?path={}",
                    urlencoding::encode(path)
                ),
            )
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {status}");
        }
        Ok(r.bytes().await.context("reading body")?)
    }

    pub async fn write_file(&self, id: &str, path: &str, data: Vec<u8>) -> Result<()> {
        let r = self
            .req(
                Method::PUT,
                &format!(
                    "/api/servers/{id}/files/write?path={}",
                    urlencoding::encode(path)
                ),
            )
            .body(data)
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn mkdir(&self, id: &str, path: &str) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/files/mkdir"))
            .json(&serde_json::json!({"path": path}))
            .send()
            .await?;
        ensure_ok(r).await
    }

    /// Ask the daemon to download a remote file into the server data dir.
    pub async fn fetch_file(&self, id: &str, url: &str, path: &str) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/files/fetch"))
            .timeout(std::time::Duration::from_secs(660))
            .json(&serde_json::json!({"url": url, "path": path}))
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn delete_path(&self, id: &str, path: &str) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/files/delete"))
            .json(&serde_json::json!({"path": path}))
            .send()
            .await?;
        ensure_ok(r).await
    }

    /// Upload a modpack zip and kick off the installer job.
    pub async fn upload_pack(&self, id: &str, filename: &str, data: Vec<u8>) -> Result<()> {
        let r = self
            .req(
                Method::POST,
                &format!(
                    "/api/servers/{id}/install/pack?filename={}",
                    urlencoding::encode(filename)
                ),
            )
            .body(data)
            .timeout(std::time::Duration::from_secs(300))
            .send()
            .await?;
        if r.status() == reqwest::StatusCode::ACCEPTED || r.status().is_success() {
            return Ok(());
        }
        ensure_ok(r).await
    }

    pub async fn install_status(&self, id: &str) -> Result<nucleus_core::InstallStatus> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/install/status"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn create_backup(&self, id: &str) -> Result<serde_json::Value> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/backups"))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn list_backups(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/backups"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn delete_backup(&self, id: &str, bid: &str) -> Result<()> {
        let r = self
            .req(Method::DELETE, &format!("/api/servers/{id}/backups/{bid}"))
            .send()
            .await?;
        ensure_ok(r).await
    }

    /// Download a backup archive as raw bytes (used during node transfer).
    pub async fn download_backup_bytes(&self, id: &str, bid: &str) -> Result<bytes::Bytes> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/backups/{bid}"))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {status}");
        }
        Ok(r.bytes().await.context("reading backup body")?)
    }

    /// Push a tar.gz archive (raw bytes) into the destination server's data dir.
    pub async fn upload_transfer(&self, id: &str, data: Vec<u8>) -> Result<()> {
        let r = self
            .req(Method::PUT, &format!("/api/servers/{id}/transfer"))
            .timeout(std::time::Duration::from_secs(600))
            .body(data)
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn restore_backup(&self, id: &str, bid: &str) -> Result<()> {
        let r = self
            .req(
                Method::POST,
                &format!("/api/servers/{id}/backups/{bid}/restore"),
            )
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub fn backup_download_url(&self, id: &str, bid: &str) -> String {
        format!("{}/api/servers/{id}/backups/{bid}", self.base)
    }

    /// Long-timeout authenticated GET for streaming large payloads (backups).
    pub async fn get_stream(&self, url: &str) -> reqwest::Result<reqwest::Response> {
        let path = url.strip_prefix(self.base.as_str()).unwrap_or(url);
        let ts = chrono::Utc::now().timestamp();
        self.http
            .get(url)
            .bearer_auth(&self.token)
            .header("x-nucleus-timestamp", ts.to_string())
            .header("x-nucleus-signature", self.sign("GET", path, ts))
            .timeout(std::time::Duration::from_secs(3600))
            .send()
            .await
    }

    pub fn ws_console_url(&self, id: &str) -> String {
        let mut base = self.base.clone();
        if base.starts_with("https://") {
            base = format!("wss://{}", &base[8..]);
        } else if base.starts_with("http://") {
            base = format!("ws://{}", &base[7..]);
        } else {
            base = format!("ws://{base}");
        }
        format!("{base}/api/servers/{id}/ws")
    }

    pub async fn ai_diagnose(&self, id: &str, note: Option<&str>) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "note": note });
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/ai/diagnose"))
            .timeout(std::time::Duration::from_secs(300))
            .json(&body)
            .send()
            .await?;
        parse(r).await
    }

    pub async fn ai_incidents(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/ai/incidents"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn update_config(&self, id: &str, body: &serde_json::Value) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/config"))
            .json(body)
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn stats(&self, id: &str) -> Result<serde_json::Value> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/stats"))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn schedules(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/schedules"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn schedule_add(
        &self,
        id: &str,
        name: &str,
        cron: &str,
        action: &str,
        payload: Option<&str>,
    ) -> Result<serde_json::Value> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/schedules"))
            .json(&serde_json::json!({
                "name": name, "cron": cron, "action": action,
                "payload": payload, "enabled": true,
            }))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn schedule_toggle(
        &self,
        id: &str,
        tid: &str,
        enabled: bool,
    ) -> Result<serde_json::Value> {
        let r = self
            .req(
                Method::PUT,
                &format!("/api/servers/{id}/schedules/{tid}"),
            )
            .json(&serde_json::json!({ "enabled": enabled }))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn schedule_delete(&self, id: &str, tid: &str) -> Result<()> {
        let r = self
            .req(
                Method::DELETE,
                &format!("/api/servers/{id}/schedules/{tid}"),
            )
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn schedule_run(&self, id: &str, tid: &str) -> Result<()> {
        let r = self
            .req(
                Method::POST,
                &format!("/api/servers/{id}/schedules/{tid}/run"),
            )
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn rerun_install_script(&self, id: &str, image: Option<String>) -> Result<()> {
        let mut req = self.req(Method::POST, &format!("/api/servers/{id}/install/script"));
        if let Some(img) = image {
            req = req.json(&serde_json::json!({ "image": img }));
        }
        let r = req.send().await?;
        ensure_ok(r).await
    }

    pub async fn sftp_info(&self, id: &str) -> Result<serde_json::Value> {
        let r = self
            .req(Method::GET, &format!("/api/servers/{id}/sftp"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn sftp_reset(&self, id: &str) -> Result<serde_json::Value> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/sftp/reset"))
            .send()
            .await?;
        parse(r).await
    }

    pub async fn rename_path(&self, id: &str, body: &serde_json::Value) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/files/rename"))
            .json(body)
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn archive(&self, id: &str, req: &crate::routes::proxy::ArchiveReq) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/files/archive"))
            .json(req)
            .send()
            .await?;
        ensure_ok(r).await
    }

    pub async fn extract(&self, id: &str, req: &crate::routes::proxy::ArchiveReq) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/files/extract"))
            .json(req)
            .send()
            .await?;
        ensure_ok(r).await
    }
}

async fn ensure_ok(r: reqwest::Response) -> Result<()> {
    if r.status().is_success() || r.status() == reqwest::StatusCode::ACCEPTED {
        Ok(())
    } else {
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        Err(anyhow::anyhow!("HTTP {status}: {text}"))
    }
}

async fn parse<T: serde::de::DeserializeOwned>(r: reqwest::Response) -> Result<T> {
    let status = r.status();
    if !(status.is_success() || status == reqwest::StatusCode::ACCEPTED) {
        let text = r.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {text}");
    }
    r.json().await.context("decoding daemon response")
}

/// Cert verifier that accepts anything — only used when an admin explicitly
/// marks a node `tls_insecure` (self-signed daemon certificates).
#[derive(Debug)]
struct AcceptAnyVerifier;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme as S;
        vec![
            S::RSA_PKCS1_SHA256,
            S::RSA_PKCS1_SHA384,
            S::RSA_PKCS1_SHA512,
            S::RSA_PSS_SHA256,
            S::RSA_PSS_SHA384,
            S::RSA_PSS_SHA512,
            S::ECDSA_NISTP256_SHA256,
            S::ECDSA_NISTP384_SHA384,
            S::ED25519,
        ]
    }
}
