use super::{AssistantTurn, ChatMessage, ToolCall};
use anyhow::{anyhow, Context, Result};

pub struct AnthropicClient {
    base_url: String,
    api_key: String,
}

const VERSION_HEADER: &str = "2023-06-01";

impl AnthropicClient {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        Self {
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
            api_key,
        }
    }

    pub async fn complete(
        &self,
        http: &reqwest::Client,
        model: &str,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<AssistantTurn> {
        // Anthropic wants a top-level system string and strict user/assistant alternation.
        let mut system = String::new();
        let mut wire: Vec<(String, serde_json::Value)> = Vec::new();

        for m in messages {
            match m {
                ChatMessage::System(s) => {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(s);
                }
                ChatMessage::User(s) => {
                    wire.push(("user".into(), serde_json::json!([{"type":"text","text":s}])))
                }
                ChatMessage::Assistant { text, tool_calls } => {
                    let mut blocks = Vec::new();
                    if let Some(t) = text {
                        blocks.push(serde_json::json!({"type":"text","text":t}));
                    }
                    for c in tool_calls {
                        blocks.push(serde_json::json!({
                            "type":"tool_use",
                            "id": c.id,
                            "name": c.name,
                            "input": c.args,
                        }));
                    }
                    wire.push(("assistant".into(), serde_json::Value::Array(blocks)));
                }
                ChatMessage::ToolResult { call_id, content } => {
                    let block = serde_json::json!({
                        "type":"tool_result",
                        "tool_use_id": call_id,
                        "content": content,
                    });
                    // Merge consecutive tool results into one user message.
                    match wire.last_mut() {
                        Some((role, blocks)) if role == "user" => {
                            blocks.as_array_mut().unwrap().push(block);
                        }
                        _ => wire.push(("user".into(), serde_json::Value::Array(vec![block]))),
                    }
                }
            }
        }

        let msgs: Vec<serde_json::Value> = wire
            .into_iter()
            .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
            .collect();

        let tools_val: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t["name"],
                    "description": t["description"],
                    "input_schema": t["input_schema"],
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": 2048,
            "messages": msgs,
            "tools": tools_val,
        });
        if !system.is_empty() {
            body["system"] = serde_json::Value::String(system);
        }

        let resp = http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", VERSION_HEADER)
            .json(&body)
            .send()
            .await
            .context("anthropic-compatible request failed")?;

        let status = resp.status();
        let raw: serde_json::Value = resp.json().await.context("parsing provider response")?;
        if !status.is_success() {
            let msg = raw["error"]["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("provider HTTP {status}: {msg}"));
        }

        let mut text = None;
        let mut calls = Vec::new();
        if let Some(blocks) = raw["content"].as_array() {
            for b in blocks {
                match b["type"].as_str() {
                    Some("text") => text = b["text"].as_str().map(str::to_owned),
                    Some("tool_use") => calls.push(ToolCall {
                        id: b["id"].as_str().unwrap_or_default().to_string(),
                        name: b["name"].as_str().unwrap_or_default().to_string(),
                        args: b["input"].clone(),
                    }),
                    _ => {}
                }
            }
        }
        Ok(AssistantTurn {
            text,
            tool_calls: calls,
        })
    }
}
