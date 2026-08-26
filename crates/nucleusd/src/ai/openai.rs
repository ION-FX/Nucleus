use super::{AssistantTurn, ChatMessage, ToolCall};
use anyhow::{anyhow, Context, Result};

pub struct OpenAiClient {
    base_url: String,
    api_key: String,
}

impl OpenAiClient {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        Self {
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
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
        let wire: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| match m {
                ChatMessage::System(s) => {
                    serde_json::json!({"role":"system","content":s})
                }
                ChatMessage::User(s) => serde_json::json!({"role":"user","content":s}),
                ChatMessage::Assistant { text, tool_calls } => {
                    let mut msg = serde_json::json!({"role":"assistant"});
                    if let Some(t) = text {
                        msg["content"] = serde_json::Value::String(t.clone());
                    } else {
                        msg["content"] = serde_json::Value::Null;
                    }
                    if !tool_calls.is_empty() {
                        let calls: Vec<serde_json::Value> = tool_calls
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        "arguments": c.args.to_string(),
                                    },
                                })
                            })
                            .collect();
                        msg["tool_calls"] = serde_json::Value::Array(calls);
                    }
                    msg
                }
                ChatMessage::ToolResult { call_id, content } => {
                    serde_json::json!({"role":"tool","tool_call_id":call_id,"content":content})
                }
            })
            .collect();

        let tools_val: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t["name"],
                        "description": t["description"],
                        "parameters": t["parameters"],
                    },
                })
            })
            .collect();

        let body = serde_json::json!({
            "model": model,
            "messages": wire,
            "tools": tools_val,
            "tool_choice": "auto",
        });

        let resp = http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("openai-compatible request failed")?;

        let status = resp.status();
        let raw: serde_json::Value = resp.json().await.context("parsing provider response")?;
        if !status.is_success() {
            let msg = raw["error"]["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("provider HTTP {status}: {msg}"));
        }

        let message = &raw["choices"][0]["message"];
        let text = message["content"].as_str().map(str::to_owned);
        let mut calls = Vec::new();
        if let Some(arr) = message["tool_calls"].as_array() {
            for c in arr {
                let args_raw = c["function"]["arguments"].as_str().unwrap_or("{}");
                let args: serde_json::Value =
                    serde_json::from_str(args_raw).unwrap_or(serde_json::json!({}));
                calls.push(ToolCall {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    name: c["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    args,
                });
            }
        }
        Ok(AssistantTurn {
            text,
            tool_calls: calls,
        })
    }
}
