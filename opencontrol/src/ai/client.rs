use anyhow::{Context, Result};
use rig::{
    client::CompletionClient,
    completion::{CompletionModel, Message, message::AssistantContent},
    providers::openrouter,
};

pub use opencontrol_sdk::ToolDefinition;

#[derive(Clone)]
pub struct AiClient {
    rig_client: openrouter::Client,
    #[allow(dead_code)]
    api_url: String,
}

impl AiClient {
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let rig_client = openrouter::Client::new(api_key.into())
            .context("Failed to create OpenRouter client")?;
        Ok(Self {
            rig_client,
            api_url: api_url.into(),
        })
    }

    pub async fn chat(
        &self,
        model_name: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<ChatResponse> {
        let model = self.rig_client.completion_model(model_name);

        // Pass the last message as the prompt (preserves all content parts including images)
        let current_message = messages.last().context("No message content")?.clone();

        let mut request = model.completion_request(current_message);

        // Include history (all messages except the last one)
        let history: Vec<_> = messages
            .iter()
            .take(messages.len().saturating_sub(1))
            .cloned()
            .collect();
        if !history.is_empty() {
            request = request.messages(history);
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                let rig_tools: Vec<_> = tools
                    .iter()
                    .map(|t| rig::completion::ToolDefinition {
                        name: t.function.name.clone(),
                        description: t.function.description.clone(),
                        parameters: t.function.parameters.clone(),
                    })
                    .collect();
                request = request.tools(rig_tools);
            }
        }

        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Completion error: {:?}", e))?;

        let mut tool_calls_vec: Vec<ToolCall> = Vec::new();
        let mut content_parts: Vec<String> = Vec::new();

        for c in response.choice.iter() {
            match c {
                AssistantContent::Text(t) => content_parts.push(t.text.clone()),
                AssistantContent::ToolCall(tc) => {
                    tool_calls_vec.push(ToolCall {
                        id: tc.id.clone(),
                        kind: "function".to_string(),
                        function: ToolCallFunction {
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.to_string(),
                        },
                    });
                }
                _ => {}
            }
        }

        let tool_calls = if tool_calls_vec.is_empty() {
            None
        } else {
            Some(tool_calls_vec)
        };

        let content = content_parts.join("");
        let has_tool_calls = tool_calls.is_some();

        Ok(ChatResponse {
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            tool_calls,
            finish_reason: if has_tool_calls {
                Some("tool_calls".to_string())
            } else {
                Some("stop".to_string())
            },
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub finish_reason: Option<String>,
}
