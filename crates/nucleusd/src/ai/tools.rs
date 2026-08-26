use crate::state::{AppState, ServerRuntime};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::Arc;

/// Execute an AI tool call against the daemon's local capabilities.
pub async fn execute_tool(
    state: &Arc<AppState>,
    rt: &Arc<ServerRuntime>,
    name: &str,
    args: &Value,
) -> Result<String> {
    let id = rt.spec.id.as_str();
    match name {
        "get_status" => {
            let st = crate::docker::status(state.clone(), id).await?;
            Ok(serde_json::to_string(&st)?)
        }
        "get_recent_logs" => {
            let tail = args.get("tail").and_then(|v| v.as_u64()).unwrap_or(200);
            let tail = tail.clamp(10, 1000) as usize;
            Ok(rt.recent_logs(tail).join("\n"))
        }
        "power_action" => {
            if !state.cfg.ai.allow_power_actions {
                return Err(anyhow!(
                    "power actions are disabled by the operator (ai.allow_power_actions=false)"
                ));
            }
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing action"))?;
            let action = match action {
                "start" => nucleus_core::PowerAction::Start,
                "stop" => nucleus_core::PowerAction::Stop,
                "kill" => nucleus_core::PowerAction::Kill,
                "restart" => nucleus_core::PowerAction::Restart,
                other => return Err(anyhow!("unknown power action {other}")),
            };
            // Type-erased to break a cyclic future size (power -> watcher -> ai -> power).
            let fut: std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>,
            > = Box::pin(crate::docker::power(state.clone(), id, action, None));
            fut.await?;
            Ok(format!("{action:?} executed"))
        }
        "send_console_command" => {
            let cmd = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing command"))?;
            // Sending a bare `stop` is effectively a power action; gate it too.
            if cmd.trim().eq_ignore_ascii_case("stop") && !state.cfg.ai.allow_power_actions {
                return Err(anyhow!(
                    "stopping the server is a power action and is disabled"
                ));
            }
            crate::docker::send_command(state.clone(), id, cmd).await?;
            Ok(format!("sent: {cmd}"))
        }
        "list_files" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");
            let entries = crate::files::list_files_inner(state, id, Some(path.to_string())).await?;
            let lines: Vec<String> = entries
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {}",
                        if e.is_dir { "dir " } else { "file" },
                        e.path,
                        e.size
                    )
                })
                .collect();
            Ok(lines.join("\n"))
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing path"))?;
            let max = args
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(8192)
                .clamp(256, 65536) as usize;
            read_file_text(state, id, path, max).await
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

pub async fn read_file_text(
    state: &Arc<AppState>,
    id: &str,
    path: &str,
    max_bytes: usize,
) -> Result<String> {
    use std::os::unix::fs::FileExt;
    let root = {
        let rt = state.get(id)?;
        rt.server_dir(&state.cfg)
    };
    let file = crate::files::safe_join(&root, path)?;
    let f = std::fs::File::open(&file)?;
    let len = f.metadata()?.len() as usize;
    let cap = max_bytes.min(len);
    let mut buf = vec![0u8; cap];
    f.read_exact_at(&mut buf, 0)?;
    let mut text = String::from_utf8_lossy(&buf).to_string();
    if len > cap {
        text.push_str(&format!("\n… (truncated, {len} bytes total)"));
    }
    Ok(text)
}
