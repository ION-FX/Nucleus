pub mod anthropic;
pub mod openai;
pub mod tools;

use crate::config::{AiConfig, AiProvider};
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub struct AssistantTurn {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// A provider-agnostic LLM client speaking either the OpenAI or Anthropic wire protocol.
pub enum Provider {
    OpenAi(openai::OpenAiClient),
    Anthropic(anthropic::AnthropicClient),
}

impl Provider {
    pub fn from_config(cfg: &AiConfig) -> Result<Self> {
        let key = AiConfigProvider::resolve_key(cfg)
            .ok_or_else(|| anyhow!("ai.api_key is not configured"))?;
        match cfg.provider {
            AiProvider::OpenAi => Ok(Self::OpenAi(openai::OpenAiClient::new(
                cfg.base_url.clone(),
                key,
            ))),
            AiProvider::Anthropic => Ok(Self::Anthropic(anthropic::AnthropicClient::new(
                cfg.base_url.clone(),
                key,
            ))),
        }
    }

    pub async fn complete(
        &self,
        http: &reqwest::Client,
        model: &str,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<AssistantTurn> {
        match self {
            Provider::OpenAi(c) => c.complete(http, model, messages, tools).await,
            Provider::Anthropic(c) => c.complete(http, model, messages, tools).await,
        }
    }
}

struct AiConfigProvider;

impl AiConfigProvider {
    fn resolve_key(cfg: &AiConfig) -> Option<String> {
        crate::config::Config::resolve_secret(&Some(cfg.api_key.clone()))
    }
}

#[derive(Serialize)]
pub struct IncidentReport {
    pub server_id: String,
    pub trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub started_at: String,
    pub finished_at: String,
    pub summary: String,
    pub actions: Vec<String>,
    pub tool_rounds: u32,
}

const SYSTEM_PROMPT: &str = r#"You are Nucleus AI, the autonomous system administrator for a game server managed by the Nucleus panel.

Your job:
1. Diagnose crashes and abnormal behaviour by inspecting logs first (get_recent_logs).
2. Identify the root cause: plugin/mod errors, missing configs, out-of-memory kills, bad startup flags, corrupted data.
3. Fix what you safely can with your tools: edit-free fixes like restarting (power_action), sending console commands (send_console_command), and reading config files to confirm problems (list_files / read_file).
4. Prefer minimal interventions. Restart once after identifying a transient fault. Never repeat a restart more than twice without changing something.
5. NEVER delete world/player data or backups. Never modify files outside this server's directory. read_file is read-only; you cannot write files - if a config must change, report exactly which file and what change is needed.
6. Finish with a concise summary: root cause, actions taken (if any), and recommended manual follow-ups."#;

pub fn tool_definitions() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "get_status",
            "description": "Get current server status: running state, exit code.",
            "input_schema": {"type": "object", "properties": {}},
            "parameters": {"type": "object", "properties": {}}
        }),
        serde_json::json!({
            "name": "get_recent_logs",
            "description": "Fetch recent console log lines (newest last).",
            "input_schema": {"type":"object","properties":{"tail":{"type":"integer","description":"Number of lines (default 200, max 1000)"}}},
            "parameters": {"type":"object","properties":{"tail":{"type":"integer"}}}
        }),
        serde_json::json!({
            "name": "power_action",
            "description": "Perform start/stop/kill/restart on the server container.",
            "input_schema": {"type":"object","required":["action"],"properties":{"action":{"type":"string","enum":["start","stop","kill","restart"]}}},
            "parameters": {"type":"object","required":["action"],"properties":{"action":{"type":"string","enum":["start","stop","kill","restart"]}}}
        }),
        serde_json::json!({
            "name": "send_console_command",
            "description": "Send a command to the server process stdin (game console), e.g. 'stop' or 'whitelist add bob'.",
            "input_schema": {"type":"object","required":["command"],"properties":{"command":{"type":"string"}}},
            "parameters": {"type":"object","required":["command"],"properties":{"command":{"type":"string"}}}
        }),
        serde_json::json!({
            "name": "list_files",
            "description": "List files in a directory of the server data volume ('/' = server root).",
            "input_schema": {"type":"object","properties":{"path":{"type":"string"}}},
            "parameters": {"type":"object","properties":{"path":{"type":"string"}}}
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "Read up to max_bytes of a text file from the server data volume.",
            "input_schema": {"type":"object","required":["path"],"properties":{"path":{"type":"string"},"max_bytes":{"type":"integer"}}},
            "parameters": {"type":"object","required":["path"],"properties":{"path":{"type":"string"},"max_bytes":{"type":"integer"}}}
        }),
    ]
}

/// Run a full agent investigation for a server. Returns the incident report.
pub async fn diagnose(
    state: Arc<AppState>,
    id: &str,
    trigger: &str,
    note: Option<String>,
) -> Result<IncidentReport> {
    if !state.cfg.ai.enabled {
        return Err(anyhow!("AI support is disabled in nucleusd config"));
    }
    let rt = state.get(id)?;
    let started_at = chrono::Utc::now().to_rfc3339();

    // Serialize incidents per server.
    let busy = state
        .ai_busy
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = busy.lock().await;

    let provider = Provider::from_config(&state.cfg.ai)?;
    let status = crate::docker::status(state.clone(), id)
        .await
        .unwrap_or_else(|_| rt.status());

    let mut context = format!(
        "Server: {}\nImage: {}\nStartup: {}\nRunning: {} (exit code: {:?})\nTrigger: {}\n",
        rt.spec.name, rt.spec.image, rt.spec.startup, status.running, status.exit_code, trigger,
    );
    context.push_str(&format!(
        "\nRecent console output:\n{}\n",
        rt.recent_logs(150).join("\n")
    ));
    if let Some(n) = note {
        context.push_str(&format!("\nOperator note: {n}\n"));
    }

    let tools = tool_definitions();
    let mut messages = vec![
        ChatMessage::System(SYSTEM_PROMPT.to_string()),
        ChatMessage::User(context),
    ];

    let mut actions = Vec::new();
    let mut rounds: u32 = 0;
    let mut final_text = String::from("(no response)");

    while rounds < state.cfg.ai.max_tool_rounds {
        rounds += 1;
        let turn = provider
            .complete(&state.http, &state.cfg.ai.model, &messages, &tools)
            .await?;

        let calls = turn.tool_calls.clone();
        if calls.is_empty() {
            final_text = turn.text.unwrap_or(final_text);
            break;
        }

        if let Some(t) = &turn.text {
            final_text = t.clone();
        }

        let mut results = Vec::new();
        for call in &calls {
            let outcome = tools::execute_tool(&state, &rt, &call.name, &call.args).await;
            match &outcome {
                Ok(text) if call.name == "power_action" || call.name == "send_console_command" => {
                    actions.push(format!("{}({}) -> ok", call.name, call.args));
                    rt.push_log(&format!("[nucleus-ai] executed {}: {}", call.name, text));
                }
                Err(e) => {
                    rt.push_log(&format!("[nucleus-ai] tool {} failed: {e}", call.name));
                }
                _ => {}
            }
            results.push(ChatMessage::ToolResult {
                call_id: call.id.clone(),
                content: outcome.unwrap_or_else(|e| format!("error: {e:#}")),
            });
        }
        messages.push(ChatMessage::Assistant {
            text: None,
            tool_calls: calls,
        });
        for r in results {
            messages.push(r);
        }
    }

    let report = IncidentReport {
        server_id: id.to_string(),
        trigger: trigger.to_string(),
        exit_code: *rt.exit_code.lock().unwrap(),
        started_at,
        finished_at: chrono::Utc::now().to_rfc3339(),
        summary: final_text,
        actions,
        tool_rounds: rounds,
    };

    persist_incident(&state.cfg, id, &report);
    rt.push_log(&format!(
        "[nucleus-ai] incident complete: {}",
        truncate(&report.summary, 160)
    ));
    Ok(report)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn persist_incident(cfg: &crate::config::Config, id: &str, report: &IncidentReport) {
    let dir = cfg.data_dir.join("ai").join(id);
    let _ = std::fs::create_dir_all(&dir);
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    if let Ok(json) = serde_json::to_string_pretty(report) {
        let _ = std::fs::write(dir.join(format!("incident-{ts}.json")), json);
    }
}

pub fn list_incidents(cfg: &crate::config::Config, id: &str) -> Vec<serde_json::Value> {
    let dir = cfg.data_dir.join("ai").join(id);
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if let Ok(raw) = std::fs::read_to_string(e.path()) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    out.push(v);
                }
            }
        }
    }
    out.reverse();
    out
}
