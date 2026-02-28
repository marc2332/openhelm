use anyhow::{Context, Result};
use futures::StreamExt;
use rig::{
    client::CompletionClient,
    completion::{CompletionModel, Message, message::AssistantContent},
    providers::{openai, openrouter},
    streaming::StreamedAssistantContent,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use openhelm_sdk::ToolDefinition;

use crate::config::AiConfig;

#[derive(Clone)]
enum Provider {
    OpenRouter(openrouter::Client),
    OpenAi(openai::CompletionsClient),
}

#[derive(Clone)]
pub struct AiClient {
    provider: Provider,
}

/// Events emitted by the streaming chat method.
#[derive(Debug)]
pub enum StreamEvent {
    /// A chunk of text from the model.
    TextDelta(String),
    /// A complete tool call.
    ToolCall(ToolCall),
    /// The stream has finished.
    Done { finish_reason: Option<FinishReason> },
    /// A fatal error.
    Error(String),
}

impl AiClient {
    /// Create a new AI client.
    ///
    /// When `ai_config.is_openai()` is true the OpenAI Completions API provider
    /// is used; otherwise OpenRouter.  A custom `api_url` (if set) overrides the
    /// provider's default base URL.
    pub fn new(ai_config: &AiConfig) -> Result<Self> {
        let api_key = &ai_config.api_key;
        let api_url = ai_config.api_url.as_deref();

        let provider = if ai_config.is_openai() {
            let mut builder = openai::CompletionsClient::builder().api_key(api_key);
            if let Some(url) = api_url {
                builder = builder.base_url(url);
            }
            let client = builder.build().context("Failed to create OpenAI client")?;
            info!(url = ai_config.effective_api_url(), "Using OpenAI provider");
            Provider::OpenAi(client)
        } else {
            let mut builder = openrouter::Client::builder().api_key(api_key);
            if let Some(url) = api_url {
                builder = builder.base_url(url);
            }
            let client = builder
                .build()
                .context("Failed to create OpenRouter client")?;
            info!(
                url = ai_config.effective_api_url(),
                "Using OpenRouter provider"
            );
            Provider::OpenRouter(client)
        };

        Ok(Self { provider })
    }

    /// Streaming chat - returns a channel receiver that yields [`StreamEvent`]s.
    ///
    /// Text chunks are sent as they arrive from the provider.  Tool calls are
    /// accumulated from deltas and emitted as complete [`StreamEvent::ToolCall`]
    /// events once fully assembled.  A final [`StreamEvent::Done`] is always
    /// sent before the channel closes.
    pub async fn chat_stream(
        &self,
        model_name: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        match &self.provider {
            Provider::OpenRouter(client) => {
                Self::stream_with_model(client.completion_model(model_name), messages, tools).await
            }
            Provider::OpenAi(client) => {
                Self::stream_with_model(client.completion_model(model_name), messages, tools).await
            }
        }
    }

    async fn stream_with_model<M>(
        model: M,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<mpsc::Receiver<StreamEvent>>
    where
        M: CompletionModel + Send + 'static,
        M::StreamingResponse: Send + Unpin + 'static,
    {
        let current_message = messages.last().context("No message content")?.clone();
        let mut request = model.completion_request(current_message);

        let history: Vec<_> = messages
            .iter()
            .take(messages.len().saturating_sub(1))
            .cloned()
            .collect();
        if !history.is_empty() {
            request = request.messages(history);
        }

        if let Some(tools) = tools.filter(|t| !t.is_empty()) {
            request = request.tools(Self::to_rig_tools(tools));
        }

        let mut stream = request
            .stream()
            .await
            .map_err(|err| anyhow::anyhow!("Stream error: {:?}", err))?;

        let (tx, rx) = mpsc::channel::<StreamEvent>(64);

        tokio::spawn(async move {
            let mut tool_calls: Vec<ToolCall> = Vec::new();

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(StreamedAssistantContent::Text(text)) => {
                        if tx.send(StreamEvent::TextDelta(text.text)).await.is_err() {
                            return;
                        }
                    }
                    Ok(StreamedAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id: _,
                    }) => {
                        let tc = ToolCall {
                            id: tool_call.id,
                            kind: "function".to_string(),
                            function: ToolCallFunction {
                                name: tool_call.function.name,
                                arguments: tool_call.function.arguments.to_string(),
                            },
                        };
                        tool_calls.push(tc);
                    }
                    Ok(StreamedAssistantContent::Final(_)) => {
                        // Final usage info - we don't surface it for now.
                    }
                    Ok(_) => {
                        // ToolCallDelta, Reasoning, ReasoningDelta - ignore for now.
                    }
                    Err(e) => {
                        let msg = format!("{:?}", e);
                        if msg.contains("aborted") {
                            break;
                        }
                        warn!(error = %msg, "Stream chunk error");
                        let _ = tx.send(StreamEvent::Error(msg)).await;
                        return;
                    }
                }
            }

            // After the stream ends, emit any accumulated tool calls.
            for tc in tool_calls {
                if tx.send(StreamEvent::ToolCall(tc)).await.is_err() {
                    return;
                }
            }

            // Determine finish reason from the aggregated stream response.
            // The rig streaming API doesn't directly expose finish_reason in a
            // typed way, so we infer it from what we collected.
            let finish_reason = if !stream
                .choice
                .iter()
                .any(|c| matches!(c, AssistantContent::ToolCall(_)))
            {
                Some(FinishReason::Stop)
            } else {
                Some(FinishReason::ToolCalls)
            };

            let _ = tx.send(StreamEvent::Done { finish_reason }).await;
        });

        Ok(rx)
    }

    fn to_rig_tools(tools: &[ToolDefinition]) -> Vec<rig::completion::ToolDefinition> {
        tools
            .iter()
            .map(|tool| rig::completion::ToolDefinition {
                name: tool.function.name.clone(),
                description: tool.function.description.clone(),
                parameters: tool.function.parameters.clone(),
            })
            .collect()
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

#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    Stop,
    /// Not currently emitted by the streaming path but matched by the
    /// agentic loop to detect truncated responses.
    #[allow(dead_code)]
    Length,
    ToolCalls,
}
