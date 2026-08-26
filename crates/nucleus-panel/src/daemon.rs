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
}

impl DaemonClient {
    pub fn new(http: reqwest::Client, node: &Node) -> Self {
        Self {
            http,
            base: node.url.trim_end_matches('/').to_string(),
            token: node.token.clone(),
        }
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(30))
    }

    pub async fn health(&self) -> Result<()> {
        let r = self.req(Method::GET, "/health").send().await?;
        if !r.status().is_success() {
            anyhow::bail!("HTTP {}", r.status());
        }
        Ok(())
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
        let r = self
            .req(Method::POST, "/api/servers")
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

    pub fn backup_download_url(&self, id: &str, bid: &str) -> String {
        format!("{}/api/servers/{id}/backups/{bid}", self.base)
    }

    pub fn auth_header_value(&self) -> String {
        format!("Bearer {}", self.token)
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

    pub async fn rerun_install_script(&self, id: &str) -> Result<()> {
        let r = self
            .req(Method::POST, &format!("/api/servers/{id}/install/script"))
            .send()
            .await?;
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
